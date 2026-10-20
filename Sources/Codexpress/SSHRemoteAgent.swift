import Foundation

enum SSHRemoteAgent {
    // Fixed program only; user fields, credentials and configuration travel on stdin.
    static let script = #"""
import base64, fcntl, hashlib, json, os, pathlib, socket, stat, sys, tempfile, time, urllib.request, uuid
LIMIT = 8 * 1024 * 1024
NAMES = ('config.toml', 'auth.json', 'auth.json.bak')

def fail(message):
    raise ValueError(message)

def read(path):
    try:
        meta = path.lstat()
    except FileNotFoundError:
        return None
    if not stat.S_ISREG(meta.st_mode) or meta.st_size > LIMIT:
        fail('配置或认证文件不是普通文件，或超过 8 MiB；未修改。')
    fd = os.open(str(path), os.O_RDONLY | getattr(os, 'O_NOFOLLOW', 0))
    with os.fdopen(fd, 'rb') as stream:
        value = stream.read(LIMIT + 1)
    if len(value) > LIMIT:
        fail('配置或认证文件过大；未修改。')
    return value

def digest(value):
    return hashlib.sha256(value).hexdigest() if value is not None else 'absent'

def atomic(path, value):
    if path.is_symlink(): fail('拒绝覆盖符号链接。')
    if value is None:
        if path.exists(): path.unlink()
        return
    fd, temp = tempfile.mkstemp(prefix='.codexpress-', dir=str(path.parent))
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(value); stream.flush(); os.fsync(stream.fileno())
        os.replace(temp, str(path))
    finally:
        if os.path.exists(temp): os.unlink(temp)

def home_path(raw):
    if not raw or not os.path.isabs(raw) or '..' in pathlib.Path(raw).parts:
        fail('请填写远端 Codex 目录的绝对路径。')
    path = pathlib.Path(raw)
    if not path.is_dir(): fail('远端 Codex 目录不存在。')
    # User-selected CODEX_HOME may itself be a symlink; config/auth targets may not.
    return path.resolve()

def backup(path):
    root = path / '.codexpress-backups'
    if root.is_symlink(): fail('备份目录不能是符号链接。')
    root.mkdir(mode=0o700, exist_ok=True); os.chmod(str(root), 0o700)
    backup_id = time.strftime('%Y%m%d-%H%M%S') + '-' + uuid.uuid4().hex[:12]
    target = root / backup_id; target.mkdir(mode=0o700)
    manifest = {'home': str(path), 'files': {}}
    names = list(NAMES) + sorted(p.name for p in path.glob('*.config.toml'))
    if len(names) > 100: fail('远端配置档案过多；未修改。')
    total = 0
    for name in names:
        value = read(path / name)
        total += len(value or b'')
        if total > 32 * 1024 * 1024: fail('备份超过 32 MiB；未修改。')
        manifest['files'][name] = digest(value)
        if value is not None: atomic(target / name, value)
    atomic(target / 'manifest.json', json.dumps(manifest).encode())
    return backup_id

def probe(payload):
    base = payload.get('base_url', '')
    from urllib.parse import urlsplit
    parsed = urlsplit(base)
    if parsed.scheme != 'http' or parsed.path != '/v1' or parsed.username or parsed.password or parsed.query or parsed.fragment:
        fail('网关地址无效。')
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl): return None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    req = urllib.request.Request(base + '/remote-check', headers={'Authorization': 'Bearer ' + payload['key']})
    try:
        with opener.open(req, timeout=8) as response:
            if response.geturl() != base + '/remote-check': fail('网关返回了意外重定向。')
            info = json.loads(response.read(4096))
        if info.get('service') != 'codexpress' or info.get('mode') != 'pool': fail('目标不是 Codexpress 来源池入口。')
    except Exception:
        fail('远端无法连接本机网关。请检查本机 IP、入口端口、防火墙及局域网或 VPN 路由；配置未修改。')
    return {'reachable': True}

def main(payload):
    action = payload.get('action')
    if action == 'discover':
        user_home = os.path.expanduser('~')
        candidates = []
        for raw in (os.environ.get('CODEX_HOME', ''), os.path.join(user_home, '.codex')):
            if raw and os.path.isabs(raw) and os.path.isdir(raw) and raw not in candidates: candidates.append(raw)
        parts = os.environ.get('SSH_CONNECTION', '').split()
        return {'user_home': user_home, 'candidates': candidates, 'client_ip': parts[0] if len(parts) == 4 else '', 'remote_host': socket.gethostname()}
    if action == 'probe': return probe(payload)
    path = home_path(payload.get('codex_home', ''))
    lock = None
    if action in ('apply', 'restore'):
        lock_fd = os.open(str(path / '.codexpress-remote.lock'), os.O_WRONLY | os.O_CREAT | getattr(os, 'O_NOFOLLOW', 0), 0o600)
        lock = os.fdopen(lock_fd, 'w')
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    if action == 'read':
        value = read(path / 'config.toml')
        # No auth.json contents leave the server.
        return {'config': base64.b64encode(value or b'').decode(), 'digest': digest(value), 'codex_home': str(path)}
    if action == 'apply':
        probe(payload)
        value = read(path / 'config.toml')
        if digest(value) != payload.get('expected_digest'): fail('远端配置已变化，请重新预览。')
        next_config = base64.b64decode(payload['config'], validate=True)
        if len(next_config) > LIMIT: fail('配置过大；未修改。')
        backup_id = backup(path)
        if digest(read(path / 'config.toml')) != payload['expected_digest']: fail('备份期间远端配置已变化，请重新预览。')
        atomic(path / 'config.toml', next_config)
        return {'applied': True, 'backup_id': backup_id, 'codex_home': str(path)}
    if action == 'restore':
        name = payload.get('backup_id', '')
        if not name or any(c not in '0123456789abcdef-' for c in name): fail('备份标识无效。')
        root = path / '.codexpress-backups'
        target = root / name
        if root.is_symlink() or target.is_symlink() or not target.is_dir(): fail('备份目录无效。')
        manifest = json.loads(read(target / 'manifest.json') or b'{}')
        if manifest.get('home') != str(path): fail('备份不属于此 Codex 目录。')
        files = manifest.get('files', {})
        if not all(n in files for n in NAMES) or len(files) > 100: fail('备份清单无效。')
        values = {}; total = 0
        for n, expected in files.items():
            if '/' in n or '\\' in n or n not in NAMES and not n.endswith('.config.toml'): fail('备份文件名无效。')
            value = read(target / n)
            total += len(value or b'')
            if digest(value) != expected or total > 32 * 1024 * 1024: fail('备份校验失败；未还原。')
            values[n] = value
        recovery = backup(path)
        current = {n: read(path / n) for n in values}
        try:
            for n, value in values.items(): atomic(path / n, value)
        except Exception:
            for n, value in current.items(): atomic(path / n, value)
            fail('还原失败，已尝试撤回本次变更；可使用还原前备份恢复。')
        return {'restored': True, 'recovery_backup_id': recovery}
    fail('未知远端操作。')

try:
    raw = sys.stdin.buffer.read(16 * 1024 * 1024 + 1)
    if len(raw) > 16 * 1024 * 1024: fail('远端请求过大。')
    result = main(json.loads(raw))
    print(json.dumps({'ok': True, 'data': result}, ensure_ascii=False))
except ValueError as error:
    allowed = str(error)
    # Only explicitly raised, fixed product errors may cross the boundary.
    if not any(allowed.startswith(prefix) for prefix in ('配置或', '请填写', '远端', '拒绝', '备份', '网关', '目标不是', '本机', '未知', '还原', '配置过大')): allowed = '远端操作失败，配置未完成更新。'
    print(json.dumps({'ok': False, 'error': allowed}, ensure_ascii=False))
except Exception:
    print(json.dumps({'ok': False, 'error': '远端操作失败，请检查目录权限、Python 3 和连接状态。'}, ensure_ascii=False))
"""#
}
