"""Serve Zedbench's private release feed over the DGX tailnet address.

Each platform publishes on its own: `current-OS-ARCH` in the releases directory
holds the version served to that platform, falling back to `current`, so one
platform can be released before the other is built.
"""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from ipaddress import IPv4Address
import json
from pathlib import Path
import re
import shutil
import ssl

PLATFORMS = {
    ('windows', 'x86_64'): 'exe',
    ('linux', 'x86_64'): 'tar.gz',
}
VERSION = re.compile(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bind', required=True, type=IPv4Address)
    parser.add_argument('--peer', required=True, type=IPv4Address, action='append')
    for option in ('cert', 'key', 'releases'):
        parser.add_argument('--' + option, required=True, type=Path)
    parser.add_argument('--hostname', required=True)
    parser.add_argument('--port', type=int, default=18443)
    args = parser.parse_args()
    origin = f'https://{args.hostname}:{args.port}'
    peers = {str(peer) for peer in args.peer}

    def current_release(os, arch):
        """The file name of the release served to a platform, or None."""
        extension = PLATFORMS.get((os, arch))
        if extension is None:
            return None
        for name in (f'current-{os}-{arch}', 'current'):
            try:
                version = (args.releases / name).read_text().strip()
            except FileNotFoundError:
                continue
            if not VERSION.fullmatch(version):
                return None
            file_name = f'zedium-{version}-{os}-{arch}.{extension}'
            return (version, file_name) if (args.releases / file_name).is_file() else None
        return None

    class Server(ThreadingHTTPServer):
        def verify_request(self, request, address):
            return address[0] in peers

        def finish_request(self, request, address):
            request.settimeout(30)
            with tls.wrap_socket(request, server_side=True) as connection:
                super().finish_request(connection, address)

    class Feed(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path == '/health':
                self.send_body(b'ZEDIUM private update feed\n', 'text/plain')
                return
            latest = re.fullmatch(r'/([a-z]+)/([a-z0-9_]+)/latest', self.path)
            if latest:
                release = current_release(*latest.groups())
                if release is None:
                    self.send_error(503, 'No release published')
                    return
                version, file_name = release
                body = json.dumps(dict(version=version, url=f'{origin}/{file_name}')).encode()
                self.send_body(body, 'application/json')
                return
            # Only the current release of a platform is downloadable.
            served = {current_release(*platform) for platform in PLATFORMS} - {None}
            if self.path not in {'/' + file_name for _, file_name in served}:
                self.send_error(404)
                return
            with (args.releases / self.path[1:]).open('rb') as file:
                self.send_response(200)
                self.send_header('Content-Type', 'application/octet-stream')
                self.send_header('Content-Length', str(file.seek(0, 2)))
                self.end_headers()
                file.seek(0)
                shutil.copyfileobj(file, self.wfile)

        def send_body(self, body, content_type):
            self.send_response(200)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'no-store')
            self.end_headers()
            self.wfile.write(body)

    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(args.cert, args.key)
    with Server((str(args.bind), args.port), Feed) as server:
        server.serve_forever()


if __name__ == '__main__':
    main()
