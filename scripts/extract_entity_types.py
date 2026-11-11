#!/usr/bin/env python3
"""Extract what Mastodon's API entities carry.

Two sources, because neither alone is sufficient:

* `app/serializers/rest/*.rb` decides what is actually emitted. It is the
  authority on which fields exist — the TypeScript lags it (4.7.0's instance
  serializer emits `icon` and `wrapstodon`; the TypeScript mentions neither).
* `app/javascript/mastodon/api_types/*.ts` says plainly which fields are
  optional, which the serializers express as `if:` conditions that are not
  always readable without running Rails.

So: fields come from the serializers, optionality from either. Both parsers
track nesting, because these files define helper classes and inline object types
whose fields belong to those, not to the entity.

Called by scripts/build_entity_types.sh.
"""
import json
import os
import re
import sys


def strip_nested_classes(src):
    """Drop nested class bodies, keeping the outer serializer's own.

    These files define decorators and helper serializers whose attributes belong
    to those classes, and several declare one as their very first line — so
    truncating at the first nested class, as a first attempt did, discards the
    entire entity. Matched by indentation, which Mastodon is consistent about.
    """
    lines = src.split("\n")
    keep, skip_indent = [], None
    for line in lines:
        if skip_indent is not None:
            if line.strip() == "end" and len(line) - len(line.lstrip()) == skip_indent:
                skip_indent = None
            continue
        m = re.match(r"(\s+)class\s+\w+", line)
        if m:
            skip_indent = len(m.group(1))
            continue
        keep.append(line)
    return "\n".join(keep)


def ruby_serializers(src_dir):
    """{serializer_name: {"fields": {name: required}, "extends": [parent]}}"""
    out = {}
    for filename in sorted(os.listdir(src_dir)):
        if not filename.endswith(".rb") or "__" in filename:
            continue
        src = open(os.path.join(src_dir, filename)).read()

        body = strip_nested_classes(src)

        # A serializer that inherits another adds to it: CredentialAccount is an
        # Account with two more fields. Names are keyed as the files are, so
        # `REST::AccountSerializer` has to become `account`.
        parent = re.search(r"^class REST::\w+ < (?:REST::)?(\w+)", src, re.M)
        extends = []
        if parent and parent.group(1) not in ("ActiveModel", "Object"):
            name = re.sub(r"Serializer$", "", parent.group(1))
            extends = [re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()]

        fields = {}
        # attributes :a, :b, :c — possibly wrapped across lines
        for m in re.finditer(
            r"^\s*attributes\s+(.+?)(?=^\s*(?:attribute|has_|def|class|end|#)|\Z)",
            body, re.M | re.S,
        ):
            for name in re.findall(r":([a-z_0-9]+)", m.group(1)):
                fields[name] = True
        # attribute :name, key: :other, if: :condition?
        # has_one / has_many / belongs_to :name, key: :other
        for m in re.finditer(
            r"^\s*(?:attribute|has_one|has_many|belongs_to)\s+:([a-z_0-9]+)(.*)$", body, re.M
        ):
            name, rest = m.group(1), m.group(2)
            key = re.search(r"key:\s*:([a-z_0-9]+)", rest)
            fields[key.group(1) if key else name] = "if:" not in rest

        name = re.sub(r"_serializer\.rb$", "", filename)
        out[name] = {"fields": fields, "extends": extends}
    return out


def ts_interfaces(src_dir):
    """{InterfaceName: {"fields": {name: required}, "extends": [...]}} plus aliases."""
    out, alias = {}, {}
    for filename in sorted(os.listdir(src_dir)):
        if not filename.endswith(".ts"):
            continue
        src = open(os.path.join(src_dir, filename)).read()
        alias.update(re.findall(r"export type (\w+)\s*=\s*(\w+);", src))
        for m in re.finditer(
            r"(?:export )?interface (\w+)(?:\s+extends\s+([\w,\s]+?))?\s*\{(.*?)^\}",
            src, re.S | re.M,
        ):
            name, extends, body = m.group(1), m.group(2), m.group(3)
            fields, depth = {}, 0
            for line in body.split("\n"):
                stripped = line.strip()
                if stripped and not stripped.startswith(("//", "/*", "*")):
                    f = re.match(r"([a-z_][a-z_0-9]*)(\??):", stripped, re.I)
                    if f and depth == 0:
                        fields[f.group(1)] = f.group(2) != "?"
                depth += line.count("{") - line.count("}")
            out[name] = {
                "fields": fields,
                "extends": [e.strip() for e in (extends or "").split(",") if e.strip()],
            }
    return out, alias


def resolve(name, table, alias=None, seen=None):
    """Flatten inheritance into one field map."""
    seen = seen or set()
    name = (alias or {}).get(name, name)
    if name in seen or name not in table:
        return {}
    seen.add(name)
    fields = {}
    for parent in table[name]["extends"]:
        fields.update(resolve(parent, table, alias, seen))
    fields.update(table[name]["fields"])
    return fields


# Entities eunha serves, and the TypeScript interface describing each.
TS_INTERFACE = {
    "account": "ApiAccountJSON",
    "status": "ApiStatusJSON",
    "relationship": "ApiRelationshipJSON",
    "poll": "ApiPollJSON",
    "media_attachment": "BaseApiMediaAttachmentJSON",
    "custom_emoji": "ApiCustomEmojiJSON",
    "list": "ApiListJSON",
    "marker": "MarkerJSON",
    "tag": "ApiHashtagJSON",
    "announcement": "ApiAnnouncementJSON",
    "suggestion": "ApiSuggestionJSON",
    "preview_card": "ApiPreviewCardJSON",
    "instance": "ApiInstanceJSON",
    # Inherits AccountSerializer and adds `source` and `role`; the TypeScript
    # describes the plain account, which is the right base for the shared fields.
    "credential_account": "ApiAccountJSON",
    # Entities the TypeScript does not describe. Optionality comes from the
    # serializer's own `if:` conditions, which is less complete but not wrong:
    # a field marked always-present that Mastodon sometimes omits shows up as a
    # difference to look at rather than as silence.
    "notification": None,
    "context": None,
    "conversation": None,
    "filter": None,
    "filter_keyword": None,
    "familiar_followers": None,
    "featured_tag": None,
    "scheduled_status": None,
    "status_source": None,
    "status_edit": None,
    "report": None,
    "role": None,
    "rule": None,
    "search": None,
    "application": None,
    "quote": None,
    "notification_policy": None,
    "notification_request": None,
    "appeal": None,
    "account_warning": None,
    "privacy_policy": None,
    "terms_of_service": None,
    "extended_description": None,
    "translation": None,
    "annual_report": None,
    "collection": None,
    "collection_item": None,
    "domain_block": None,
    "preferences": None,
    "profile": None,
    "reaction": None,
    "language": None,
}


def main():
    ruby_dir, ts_dir, out_path = sys.argv[1], sys.argv[2], sys.argv[3]
    serializers = ruby_serializers(ruby_dir)
    interfaces, alias = ts_interfaces(ts_dir)

    out = {}
    for entity, iface in sorted(TS_INTERFACE.items()):
        emitted = resolve(entity, serializers)
        if not emitted:
            print(f"  !! {entity}: no serializer found", file=sys.stderr)
            continue
        typed = resolve(iface, interfaces, alias) if iface else {}

        always, conditional = [], []
        for field, required in emitted.items():
            # Optional if either source says so.
            if required and typed.get(field, True):
                always.append(field)
            else:
                conditional.append(field)
        out[entity] = {"always": sorted(always), "conditional": sorted(conditional)}
        print(f"{entity}: {len(always)} always + {len(conditional)} optional")

    header = {"_comment": [
        "What Mastodon's API entities carry. Regenerate with",
        "scripts/build_entity_types.sh.",
        "",
        "Fields come from app/serializers/rest/*.rb, which decides what is",
        "emitted; optionality from those plus app/javascript/mastodon/api_types",
        "/*.ts, which states it plainly.",
        "",
        "`always` is present on every response; `conditional` depends on who is",
        "asking, so its absence is not a difference.",
    ]}
    header.update(dict(sorted(out.items())))
    with open(out_path, "w") as f:
        json.dump(header, f, indent=1)
        f.write("\n")


if __name__ == "__main__":
    main()
