#!/usr/bin/env python3
"""Small deterministic upstream for connectivity checks; not a performance reference."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import socket
import sys


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        sizes = {"/health": 2, "/tiny": 0, "/1k": 1024, "/1m": 1024 * 1024}
        size = sizes.get(self.path)
        if size is None:
            self.send_error(404)
            return
        body = b"x" * size
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


class Server(ThreadingHTTPServer):
    def get_request(self):
        stream, address = super().get_request()
        stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        return stream, address


if __name__ == "__main__":
    Server(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 18081), Handler).serve_forever()
