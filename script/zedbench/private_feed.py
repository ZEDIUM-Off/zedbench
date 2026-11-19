"""Serve the private Windows qualification channel over the DGX tailnet address."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from ipaddress import IPv4Address
import json
from pathlib import Path
import re
import shutil
import ssl


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ('bind', 'peer'):
        parser.add_argument('--' + option, required=True, type=IPv4Address)
    for option in ('cert', 'key', 'releases'):
        parser.add_argument('--' + option, required=True, type=Path)
    parser.add_argument('--hostname', required=True)
    parser.add_argument('--port', type=int, default=18443)
    args = parser.parse_args()
    origin = f'https://{args.hostname}:{args.port}'

    class Server(ThreadingHTTPServer):
        def verify_request(self, request, address):
            return address[0] == str(args.peer)

        def finish_request(self, request, address):
            request.settimeout(30)
            with tls.wrap_socket(request, server_side=True) as connection:
                super().finish_request(connection, address)

    class Feed(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path == '/health':
                body = b'ZEDIUM private update feed\n'
            else:
                try:
                    version = (args.releases / 'current').read_text().strip()
                    if not re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', version):
                        raise ValueError('invalid release version')
                    installer = args.releases / f'zedium-{version}-windows-x86_64.exe'
                    with installer.open('rb') as file:
                        if self.path == '/' + installer.name:
                            self.send_response(200)
                            self.send_header('Content-Type', 'application/octet-stream')
                            self.send_header('Content-Length', str(file.seek(0, 2)))
                            self.end_headers()
                            file.seek(0)
                            shutil.copyfileobj(file, self.wfile)
                            return
                        if self.path != '/windows/x86_64/latest':
                            self.send_error(404)
                            return
                        body = json.dumps(dict(version=version, url=f'{origin}/{installer.name}')).encode()
                except (FileNotFoundError, ValueError):
                    self.send_error(503, 'No release published')
                    return
            self.send_response(200)
            self.send_header('Content-Type', 'text/plain' if self.path == '/health' else 'application/json')
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
