#!/usr/bin/env python3
"""Codexpress local Images API client; no SDK, proxy discovery or retries."""
import argparse
import base64
import json
import os
from pathlib import Path
import sys
import urllib.error
import urllib.parse
import urllib.request

LIMIT = 128 * 1024 * 1024

def config():
    path = Path(os.environ.get('CODEXPRESS_IMAGE_CONFIG', Path.home() / '.codexpress/image-client.json'))
    if not path.is_file() or path.stat().st_size > 16384:
        raise ValueError('请在 Codexpress「图片生成」页面复制接入提示词，生成本机接入配置。')
    settings = json.loads(path.read_text())
    url = urllib.parse.urlsplit(settings['base_url'])
    key = settings['api_key']
    if (url.scheme != 'http' or url.hostname not in ('127.0.0.1', 'localhost') or not url.port
            or url.username or url.password or url.query or url.fragment or url.path.rstrip('/') != '/v1'
            or not isinstance(key, str) or not 16 <= len(key) <= 256
            or not all(33 <= ord(c) <= 126 for c in key)):
        raise ValueError('本机图片生成接入配置无效，请从应用重新复制接入提示词。')
    return settings['base_url'].rstrip('/'), key

def call(endpoint, payload=None):
    base, key = config()
    request = urllib.request.Request(base + endpoint, data=None if payload is None else json.dumps(payload).encode(),
                                     headers={'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    # Refuse redirects: local credentials must never leave the configured endpoint.
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            return None
    opener.add_handler(NoRedirect())
    try:
        with opener.open(request, timeout=330) as response:
            raw = response.read(LIMIT + 1)
        if len(raw) > LIMIT:
            raise ValueError('图片响应过大，未自动重试。')
        return json.loads(raw)
    except urllib.error.HTTPError as error:
        # The gateway's error is bounded; never echo arbitrary HTTP bodies.
        raise ValueError(f'图片入口返回 HTTP {error.code}，请在 Codexpress 检查账号、额度和参数；未自动重试。') from None
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        raise ValueError('图片连接失败或超时，结果未知，可能已消耗额度；未自动重试。') from None

def image_data(path):
    path = Path(path)
    if not path.is_file() or path.stat().st_size > 10 * 1024 * 1024:
        raise ValueError('参考图片须为不超过 10 MB 的本地文件。')
    data = path.read_bytes()
    if data.startswith(b'\x89PNG\r\n\x1a\n'):
        mime = 'image/png'
    elif data.startswith(b'\xff\xd8\xff'):
        mime = 'image/jpeg'
    elif data.startswith(b'RIFF') and data[8:12] == b'WEBP':
        mime = 'image/webp'
    else:
        raise ValueError('参考图片仅支持 PNG、JPEG 和 WebP。')
    return {'image_url': f'data:{mime};base64,' + base64.b64encode(data).decode()}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    sub.add_parser('check', help='检查本机连接，不生成图片')
    for action in ('generate', 'edit'):
        command = sub.add_parser(action)
        command.add_argument('--prompt', required=True)
        command.add_argument('--out', type=Path, required=True)
        command.add_argument('--size', default='auto')
        command.add_argument('--quality', choices=('auto', 'low', 'medium', 'high'), default='auto')
        command.add_argument('--n', type=int, default=1, choices=range(1, 11))
        if action == 'edit':
            command.add_argument('--image', type=Path, action='append', required=True)
    args = parser.parse_args()
    if args.action == 'check':
        data = call('/models')
        if not any(item.get('id') == 'gpt-image-2' for item in data.get('data', [])):
            raise ValueError('图片入口未返回 gpt-image-2。')
        print('Codexpress 图片入口连接正常；未生成图片。')
        return
    if not args.prompt.strip() or len(args.prompt.encode()) > 64000:
        raise ValueError('请填写不超过 64000 字节的图片提示词。')
    out = args.out.expanduser().absolute()
    if out.suffix.lower() != '.png':
        raise ValueError('输出路径请使用 .png 扩展名。')
    paths = [out] if args.n == 1 else [out.with_name(f'{out.stem}-{n + 1}.png') for n in range(args.n)]
    if any(path.exists() for path in paths):
        raise ValueError('输出文件已存在，请选择新的文件名；尚未提交生图请求。')
    out.parent.mkdir(parents=True, exist_ok=True)
    payload = dict(model='gpt-image-2', prompt=args.prompt, size=args.size, quality=args.quality, n=args.n)
    if args.action == 'edit':
        if not 1 <= len(args.image) <= 5 or sum(p.stat().st_size for p in args.image) > 32 * 1024 * 1024:
            raise ValueError('请提供 1–5 张参考图片，总大小不超过 32 MB。')
        payload['images'] = [image_data(path) for path in args.image]
    result = call('/images/edits' if args.action == 'edit' else '/images/generations', payload)
    images = result.get('data', [])
    if len(images) != len(paths):
        raise ValueError('返回图片数量与请求不符；未自动重试。')
    decoded = []
    for item in images:
        data = base64.b64decode(item['b64_json'], validate=True)
        if not data.startswith(b'\x89PNG\r\n\x1a\n') or len(data) > 32 * 1024 * 1024:
            raise ValueError('返回图片不是有效的 PNG 内容或超过大小限制。')
        decoded.append(data)
    for path, data in zip(paths, decoded):
        with path.open('xb') as file:
            file.write(data)
        print(str(path))

if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, TypeError, OSError) as error:
        # Do not dump HTTP objects, request payloads or configuration on failure.
        print(str(error) if isinstance(error, ValueError) else '图片操作失败，请检查本机配置、输入文件和输出目录；未自动重试。', file=sys.stderr)
        sys.exit(1)
