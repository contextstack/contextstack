#!/usr/bin/env python3
"""Loopback-only OpenAI wire fixture. Logs requests, never auth headers."""
import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--ready', required=True, type=Path)
    parser.add_argument('--log', required=True, type=Path)
    parser.add_argument('--release', type=Path)
    args = parser.parse_args()
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *unused):
            pass

        def respond(self, status, body):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path != '/v1/models':
                return self.respond(404, {'error': 'unknown path'})
            self.respond(200, {'object': 'list', 'data': [{'id': 'fake-paid', 'object': 'model', 'owned_by': 'fixture'}]})

        def do_POST(self):
            if self.path != '/v1/chat/completions':
                return self.respond(404, {'error': 'unknown path'})
            size = int(self.headers.get('Content-Length', '0'))
            if size < 1 or size > 65536:
                return self.respond(413, {'error': 'bounded fixture request required'})
            try:
                body = json.loads(self.rfile.read(size))
            except ValueError:
                return self.respond(400, {'error': 'invalid JSON'})
            with lock, args.log.open('a') as out:
                out.write(json.dumps({'path': self.path, 'body': body}) + '\n')
            deadline = time.monotonic() + 30
            while args.release and not args.release.exists():
                if time.monotonic() >= deadline:
                    return self.respond(504, {'error': 'fixture release timed out'})
                time.sleep(.01)
            self.respond(200, {'id': 'fixture-completion', 'object': 'chat.completion', 'model': body.get('model'),
                'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'canned completion'}, 'finish_reason': 'stop'}],
                'usage': {'prompt_tokens': 3, 'completion_tokens': 2, 'total_tokens': 5, 'cost': 0}})

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = True
    args.ready.write_text('http://127.0.0.1:' + str(server.server_port))
    server.serve_forever()


if __name__ == '__main__':
    main()
