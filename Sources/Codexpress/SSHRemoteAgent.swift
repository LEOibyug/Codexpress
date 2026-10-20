import Foundation

enum SSHRemoteAgent {
    // Fixed program only; user fields, credentials and configuration travel on stdin.
    static let script = #"""
import base64, fcntl, hashlib, json, os, pathlib, re, socket, stat, sys, tempfile, time, urllib.request, uuid
LIMIT = 8 * 1024 * 1024
NAMES = ('config.toml', 'auth.json', 'auth.json.bak')

class AgentError(Exception): pass

def fail(message):
    raise AgentError(message)

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

def backup(path, reason):
    root = path / '.codexpress-backups'
    if root.is_symlink(): fail('备份目录不能是符号链接。')
    root.mkdir(mode=0o700, exist_ok=True); os.chmod(str(root), 0o700)
    backup_id = time.strftime('%Y%m%d-%H%M%S') + '-' + uuid.uuid4().hex[:12]
    target = root / backup_id; target.mkdir(mode=0o700)
    manifest = {'home': str(path), 'files': {}, 'reason': reason, 'created_at_ns': time.time_ns()}
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

def load_backup(path, name):
    if not isinstance(name, str) or not re.fullmatch(r'[0-9]{8}-[0-9]{6}-[0-9a-f]{12}', name): fail('备份标识无效。')
    root = path / '.codexpress-backups'
    target = root / name
    if root.is_symlink() or target.is_symlink() or not target.is_dir(): fail('备份目录无效。')
    try:
        manifest = json.loads(read(target / 'manifest.json') or b'{}')
    except (ValueError, UnicodeError):
        fail('备份清单损坏；请检查远端 .codexpress-backups。')
    if not isinstance(manifest, dict) or manifest.get('home') != str(path): fail('备份不属于此 Codex 目录。')
    files = manifest.get('files', {})
    if not isinstance(files, dict) or not all(n in files for n in NAMES) or len(files) > 100: fail('备份清单无效。')
    values = {}; total = 0
    for name, expected in files.items():
        if '/' in name or '\\' in name or name not in NAMES and not name.endswith('.config.toml'): fail('备份文件名无效。')
        value = read(target / name)
        total += len(value or b'')
        if digest(value) != expected or total > 32 * 1024 * 1024: fail('备份校验失败；未还原。')
        values[name] = value
    return values

def managed(config):
    try:
        text = (config or b'').decode('utf-8').lower()
    except UnicodeError:
        fail('备份配置不是有效 UTF-8，无法确认接入前设置。')
    # Every configuration written by this app contains these literal identifiers.
    # Conservative matches (including comments) refuse rather than guess an original.
    return any(marker in text for marker in ('codexpress', 'cxr-'))

def original(path, optional=False):
    root = path / '.codexpress-backups'
    if root.is_symlink(): fail('备份目录不能是符号链接。')
    pointer = read(root / 'original.json')
    if pointer is not None:
        try:
            info = json.loads(pointer)
        except (ValueError, UnicodeError):
            fail('原始备份记录损坏；请检查远端 .codexpress-backups。')
        if not isinstance(info, dict) or info.get('version') != 1 or info.get('home') != str(path): fail('原始备份记录无效；未还原。')
        name = info.get('backup_id')
        values = load_backup(path, name)
        if digest(values['config.toml']) != info.get('config_digest') or managed(values['config.toml']): fail('原始备份无法确认；未还原。')
        return name, values
    # Upgrade old installations from the earliest checkpoint, never the last
    # locally remembered ID. Never skip an incomplete or already-managed one.
    candidates = []
    if root.exists():
        for child in root.iterdir():
            if re.fullmatch(r'[0-9]{8}-[0-9]{6}-[0-9a-f]{12}', child.name):
                if child.is_symlink() or not child.is_dir(): fail('备份目录无效。')
                candidates.append(child)
    candidates.sort(key=lambda p: (p.name[:15], p.stat().st_mtime_ns, p.name))
    for target in candidates:
        values = load_backup(path, target.name)
        if managed(values['config.toml']): fail('最早备份已含 Codexpress 网关配置，无法确认接入前设置；请检查远端 .codexpress-backups。')
        return target.name, values
    if optional: return None
    fail('未找到接入前备份；请检查远端 .codexpress-backups。')

def pin_original(path, name, values):
    target = path / '.codexpress-backups' / 'original.json'
    if read(target) is None:
        atomic(target, json.dumps({'version': 1, 'home': str(path), 'backup_id': name, 'config_digest': digest(values['config.toml'])}).encode())

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
        baseline = original(path, optional=True)
        if baseline is None and managed(value): fail('当前已是 Codexpress 网关配置，但未找到接入前备份；未修改。')
        backup_id = backup(path, 'before_apply')
        if digest(read(path / 'config.toml')) != payload['expected_digest']: fail('备份期间远端配置已变化，请重新预览。')
        if baseline is None: baseline = (backup_id, load_backup(path, backup_id))
        pin_original(path, *baseline)
        atomic(path / 'config.toml', next_config)
        return {'applied': True, 'backup_id': backup_id, 'original_backup_id': baseline[0], 'codex_home': str(path)}
    if action == 'restore':
        if 'backup_id' in payload: fail('还原仅使用首次接入前备份，不接受上次备份标识。')
        name, values = original(path)
        pin_original(path, name, values)
        recovery = backup(path, 'before_restore')
        current = {n: read(path / n) for n in values}
        try:
            for n, value in values.items(): atomic(path / n, value)
        except Exception:
            for n, value in current.items(): atomic(path / n, value)
            fail('还原失败，已尝试撤回本次变更；可使用还原前备份恢复。')
        return {'restored': True, 'original_backup_id': name, 'recovery_backup_id': recovery}
    fail('未知远端操作。')

try:
    raw = sys.stdin.buffer.read(16 * 1024 * 1024 + 1)
    if len(raw) > 16 * 1024 * 1024: fail('远端请求过大。')
    result = main(json.loads(raw))
    print(json.dumps({'ok': True, 'data': result}, ensure_ascii=False))
except AgentError as error:
    # Only deliberately raised, fixed product errors cross the boundary.
    print(json.dumps({'ok': False, 'error': str(error)}, ensure_ascii=False))
except Exception:
    print(json.dumps({'ok': False, 'error': '远端操作失败，请检查目录权限、Python 3 和连接状态。'}, ensure_ascii=False))
"""#
}
