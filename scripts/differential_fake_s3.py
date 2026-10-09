#!/usr/bin/env python3
"""A bucket for the differential harness's eunha, in memory, on one port.

eunha keeps media in S3, and the Mastodon it is compared with keeps it on its
own disk; without somewhere to put an upload, eunha answers every media
request with an error and the comparison learns nothing. This answers the
calls eunha makes — put, get, head, delete — path-style, with no checking of
signatures, and forgets everything when it stops.

    scripts/differential_fake_s3.py 9999
"""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OBJECTS = {}


def unchunk(body):
    """`aws-chunked`: `<hex size>;chunk-signature=…\\r\\n<data>\\r\\n`, to a 0."""
    out, rest = b"", body
    while rest:
        header, _, rest = rest.partition(b"\r\n")
        size = int(header.split(b";")[0], 16)
        if size == 0:
            break
        out, rest = out + rest[:size], rest[size + 2:]
    return out


class Bucket(BaseHTTPRequestHandler):
    def do_PUT(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        sha = self.headers.get("x-amz-content-sha256") or ""
        if sha.startswith("STREAMING") or "aws-chunked" in (self.headers.get("Content-Encoding") or ""):
            body = unchunk(body)
        OBJECTS[self.path.split("?")[0]] = body
        self.send_response(200)
        self.send_header("ETag", '"0"')
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        body = OBJECTS.get(self.path.split("?")[0])
        self.send_response(200 if body is not None else 404)
        self.send_header("Content-Length", str(len(body or b"")))
        self.end_headers()
        self.wfile.write(body or b"")

    def do_HEAD(self):
        body = OBJECTS.get(self.path.split("?")[0])
        self.send_response(200 if body is not None else 404)
        self.send_header("Content-Length", str(len(body or b"")))
        self.end_headers()

    def do_DELETE(self):
        OBJECTS.pop(self.path.split("?")[0], None)
        self.send_response(204)
        self.end_headers()

    def do_POST(self):
        # `DeleteObjects`, for a batch: acknowledged, nothing more.
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        body = b'<?xml version="1.0" encoding="UTF-8"?><DeleteResult></DeleteResult>'
        self.send_response(200)
        self.send_header("Content-Type", "application/xml")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Bucket).serve_forever()
