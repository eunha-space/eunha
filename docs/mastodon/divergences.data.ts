// Reads the divergence registry at build time, so the page shows exactly what
// `cargo test` checks rather than a hand-kept copy of it.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parse } from "smol-toml";
import { createMarkdownRenderer, defineLoader } from "vitepress";

export type Kind = "addition" | "omission" | "behaviour";

export interface Divergence {
  id: string;
  kind: Kind;
  /** The fields below are rendered Markdown. */
  summary: string;
  mastodon: string;
  eunha: string;
  why: string;
  evidence: string;
  since: string;
  reviewedFor: string;
}

declare const data: Divergence[];
export { data };

const registry = fileURLToPath(
  new URL("../../divergences.toml", import.meta.url),
);

export default defineLoader({
  watch: [registry],
  async load(): Promise<Divergence[]> {
    const md = await createMarkdownRenderer(process.cwd());
    const inline = (text: string) => md.renderInline(text);
    const { divergence } = parse(readFileSync(registry, "utf8")) as {
      divergence: Record<string, string>[];
    };
    return divergence.map((entry) => ({
      id: entry.id,
      kind: entry.kind as Kind,
      summary: inline(entry.summary),
      mastodon: inline(entry.mastodon),
      eunha: inline(entry.eunha),
      why: inline(entry.why),
      evidence: entry.evidence,
      since: entry.since,
      reviewedFor: entry.reviewed_for,
    }));
  },
});
