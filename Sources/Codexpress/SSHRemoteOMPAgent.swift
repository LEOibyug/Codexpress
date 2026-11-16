import Foundation

enum SSHRemoteOMPAgent {
    // Fixed Python program. All remote paths, bundles and credentials arrive on stdin.
    static let script = #"""
import base64, fcntl, hashlib, json, os, pathlib, re, shutil, socket, stat, subprocess, sys, urllib.request, uuid
NAMES = ('models.yml', 'models.yaml', 'models.json', 'config.yml', 'config.yaml', 'settings.json')
LIMIT = 4 * 1024 * 1024
SNAPSHOT_LIMIT = 24 * 1024 * 1024
TRANSPORT_LIMIT = 8 * 1024 * 1024 - 16 * 1024
DIRECTORY = os.O_RDONLY | os.O_DIRECTORY | getattr(os, 'O_NOFOLLOW', 0)
class AgentError(Exception): pass
def fail(message): raise AgentError(message)

def selected(raw):
    if not isinstance(raw, str) or not raw.startswith('/') or '..' in pathlib.Path(raw).parts or any(ord(c) < 32 for c in raw):
        fail('请填写远端 OMP 目录的绝对路径，不可包含上级目录。')
    return '/' + os.path.normpath(raw).lstrip('/')

def directory(raw, create=False):
    # Walk via directory descriptors: no symlink component can redirect config writes.
    fd = os.open('/', DIRECTORY)
    try:
        for name in pathlib.Path(raw).parts[1:]:
            try: child = os.open(name, DIRECTORY, dir_fd=fd)
            except FileNotFoundError:
                if not create: os.close(fd); return None
                try: os.mkdir(name, 0o700, dir_fd=fd); os.fsync(fd)
                except FileExistsError: pass
                child = os.open(name, DIRECTORY, dir_fd=fd)
            os.close(fd); fd = child
        return fd
    except Exception:
        os.close(fd); raise

def child_directory(fd, name, create=False):
    if fd is None: return None
    try: return os.open(name, DIRECTORY, dir_fd=fd)
    except FileNotFoundError:
        if not create: return None
        try: os.mkdir(name, 0o700, dir_fd=fd); os.fsync(fd)
        except FileExistsError: pass
        return os.open(name, DIRECTORY, dir_fd=fd)

def read(fd, name, limit=LIMIT):
    if fd is None: return None
    try: stream = os.open(name, os.O_RDONLY | os.O_NONBLOCK | getattr(os, 'O_NOFOLLOW', 0), dir_fd=fd)
    except FileNotFoundError: return None
    with os.fdopen(stream, 'rb') as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > limit: fail('OMP 配置不是独立普通文件或超过大小限制；未修改。')
        value = source.read(limit + 1)
        if len(value) > limit: fail('OMP 配置超过大小限制；未修改。')
        return value

def encoded(value): return base64.b64encode(value).decode() if value is not None else None

def state(fd, path):
    files = {name: encoded(read(fd, name)) for name in NAMES}
    meta = child_directory(fd, '.codexpress')
    try: snapshot = encoded(read(meta, 'attachment.json', SNAPSHOT_LIMIT))
    finally:
        if meta is not None: os.close(meta)
    exists = False
    if fd is not None:
        try: os.stat('agent.db', dir_fd=fd, follow_symlinks=False); exists = True
        except FileNotFoundError: pass
    value = dict(agent_dir=path, files=files, snapshot=snapshot, agent_db_exists=exists)
    if len(json.dumps(value, separators=(',', ':')).encode()) > TRANSPORT_LIMIT: fail('OMP 配置与快照合计超过传输限制。')
    value['digest'] = hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
    return value

def decoded(value, limit):
    if value is None: return None
    if not isinstance(value, str) or len(value) > ((limit + 2) // 3) * 4: fail('OMP 配置内容过大或格式无效。')
    try: result = base64.b64decode(value, validate=True)
    except Exception: fail('OMP 配置内容不是有效 Base64。')
    if len(result) > limit: fail('OMP 配置内容过大。')
    return result

def bundle(payload):
    files = payload.get('files')
    if not isinstance(files, dict) or set(files) != set(NAMES) or 'snapshot' not in payload: fail('OMP 配置文件列表无效。')
    if len(json.dumps(dict(files=files, snapshot=payload['snapshot']), separators=(',', ':')).encode()) > TRANSPORT_LIMIT: fail('OMP 配置与快照合计超过传输限制。')
    result = {name: decoded(files[name], LIMIT) for name in NAMES}
    return result, decoded(payload['snapshot'], SNAPSHOT_LIMIT)

def atomic(fd, name, value, expected):
    # Only revert/replace bytes still owned by this operation; unrelated edits survive.
    if read(fd, name, SNAPSHOT_LIMIT if name == 'attachment.json' else LIMIT) != expected:
        fail('远端 OMP 配置已变化，请重新预览。')
    if value == expected: return
    if value is None:
        os.unlink(name, dir_fd=fd); os.fsync(fd); return
    temp = '.codexpress-' + uuid.uuid4().hex
    stream = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, 'O_NOFOLLOW', 0), 0o600, dir_fd=fd)
    try:
        with os.fdopen(stream, 'wb') as target:
            target.write(value); target.flush(); os.fsync(target.fileno())
        if read(fd, name, SNAPSHOT_LIMIT if name == 'attachment.json' else LIMIT) != expected:
            fail('远端 OMP 配置已变化，请重新预览。')
        os.replace(temp, name, src_dir_fd=fd, dst_dir_fd=fd); os.fsync(fd)
    finally:
        try: os.unlink(temp, dir_fd=fd)
        except FileNotFoundError: pass

def backup(meta, current):
    root = child_directory(meta, 'backups', True)
    try:
        os.fchmod(root, 0o700)
        name = uuid.uuid4().hex
        os.mkdir(name, 0o700, dir_fd=root)
        target = child_directory(root, name)
        try:
            # A private, durable recovery bundle, never a source for automatic restore.
            data = json.dumps(current, sort_keys=True).encode()
            stream = os.open('bundle.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=target)
            with os.fdopen(stream, 'wb') as output:
                output.write(data); output.flush(); os.fsync(output.fileno())
            os.fsync(target); os.fsync(root)
        finally: os.close(target)
        return name
    finally: os.close(root)

def probe(payload):
    from urllib.parse import urlsplit
    base = payload.get('base_url', '')
    parsed = urlsplit(base)
    key = payload.get('key')
    if parsed.scheme != 'http' or not parsed.hostname or parsed.path != '/omp/v1' or parsed.username or parsed.password or parsed.query or parsed.fragment or not isinstance(key, str) or not key or '\n' in key or '\r' in key:
        fail('Codexpress Agent 入口地址或共享 Key 无效。')
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl): return None
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        req = urllib.request.Request(base + '/remote-check', headers={'Authorization': 'Bearer ' + key})
        with opener.open(req, timeout=8) as response:
            if response.geturl() != base + '/remote-check': fail('入口发生重定向。')
            info = json.loads(response.read(4097))
        if not isinstance(info, dict) or info.get('service') != 'codexpress' or info.get('mode') != 'omp' or type(info.get('version')) is not int or info['version'] != 1:
            fail('入口身份不匹配。')
    except Exception:
        fail('远端无法连接 Codexpress Agent 入口。请检查地址、Key、端口及局域网或 VPN 路由；配置未修改。')


def cli_environment(path=None):
    env = dict(os.environ)
    home = os.path.expanduser('~')
    paths = [os.path.join(home, '.local', 'bin'), os.path.join(home, '.bun', 'bin'), '/opt/homebrew/bin', '/usr/local/bin', '/usr/bin', '/bin']
    env['PATH'] = os.pathsep.join(dict.fromkeys(env.get('PATH', '').split(os.pathsep) + paths))
    if path is not None:
        env.update(PI_CODING_AGENT_DIR=path, OMP_PROFILE='default', PI_PROFILE='default')
    return env

def executable():
    return shutil.which('omp', path=cli_environment()['PATH'])

def run_cli(command, path, args, timeout, capture=False, credentials=None):
    env = cli_environment(path)
    # Only native login receives the key, never argv, files or returned command logs.
    env.pop('CODEXPRESS_API_KEY', None)
    if credentials is not None:
        env.update(CODEXPRESS_API_KEY=credentials['key'], CODEXPRESS_BASE_URL=credentials['base_url'])
    try:
        result = subprocess.run([command] + args, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, env=env, cwd=path if path and os.path.isdir(path) else os.path.expanduser('~'), timeout=timeout)
    except subprocess.TimeoutExpired:
        fail('远端官方 OMP 操作超时；请检查远端网络和 OMP 状态后重试。')
    except OSError:
        fail('无法启动远端官方 OMP，请检查安装及运行依赖。')
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).decode('utf-8', errors='replace').strip()
        if credentials is not None: detail = detail.replace(credentials['key'], '[Key]')
        detail = re.sub(r'\x1b\[[0-9;]*[A-Za-z]', '', detail)[-2000:]
        fail('远端 OMP %s 失败（%d）。%s' % (' '.join(args[:2]), result.returncode, '\n' + detail if detail else '请检查 OMP 安装与运行依赖。'))
    return result.stdout if capture else None

def version(command):
    if command is None: return None
    try: return run_cli(command, None, ['--version'], 10, True).decode('utf-8').strip() or None
    except (AgentError, UnicodeError): return None

def plugin(command, path):
    try:
        value = json.loads(run_cli(command, path, ['plugin', 'list', '--json'], 20, True))
        if not isinstance(value, dict) or not isinstance(value.get('npm'), list): raise ValueError()
        return next((item for item in value['npm'] if isinstance(item, dict) and item.get('name') == '@codexpress/omp-extension'), None)
    except (ValueError, UnicodeError):
        fail('远端官方 OMP 扩展列表格式无效，请更新官方 OMP。')

def profile(fd):
    raw = read(fd, 'codexpress.json')
    try: value = json.loads(raw) if raw is not None else {}
    except (ValueError, UnicodeError): fail('远端 codexpress.json 不是有效 JSON；未修改。')
    if not isinstance(value, dict) or ('baseUrl' in value and not isinstance(value['baseUrl'], str)):
        fail('远端 codexpress.json 配置无效；未修改。')
    return raw, value

def extension_status(path, command, omp_version, installed):
    fd = directory(path)
    try: _, config = profile(fd)
    finally:
        if fd is not None: os.close(fd)
    base = config.get('baseUrl')
    result = dict(agent_dir=path, omp_executable=command, omp_version=omp_version, installed=installed is not None,
                  environment_override=bool(os.environ.get('CODEXPRESS_BASE_URL') and os.environ['CODEXPRESS_BASE_URL'] != base))
    if installed is not None and isinstance(installed.get('version'), str): result['extension_version'] = installed['version']
    if base is not None: result['base_url'] = base
    return result

def connect_extension(payload, path, install):
    command = executable()
    if command is None: fail('远端未找到官方 omp，请先安装官方 OMP。')
    probe(payload)
    package = decoded(payload.get('package'), LIMIT) if install else None
    if install and not package: fail('OMP 扩展安装包为空。')
    fd = directory(path, install)
    if fd is None: fail('远端尚未安装 Codexpress 扩展，请先安装。')
    meta = lock = None
    try:
        meta = child_directory(fd, '.codexpress', True)
        lock = os.open('remote.lock', os.O_WRONLY | os.O_NONBLOCK | os.O_CREAT | getattr(os, 'O_NOFOLLOW', 0), 0o600, dir_fd=meta)
        if not stat.S_ISREG(os.fstat(lock).st_mode) or os.fstat(lock).st_nlink != 1: fail('OMP 锁文件无效。')
        os.fchmod(lock, 0o600); fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        before, config = profile(fd)
        if install:
            packages = child_directory(meta, 'packages', True)
            try:
                name = 'codexpress-' + hashlib.sha256(package).hexdigest() + '.tgz'
                previous = read(packages, name)
                if previous is not None and previous != package: fail('远端持久扩展包内容不匹配；未覆盖。')
                atomic(packages, name, package, previous)
            finally: os.close(packages)
            package_path = os.path.join(path, '.codexpress', 'packages', name)
            try: run_cli(command, path, ['plugin', 'install', '@codexpress/omp-extension@file:' + package_path], 100)
            except AgentError as error:
                fail('官方 OMP 扩展安装失败或超时；持久包已保留，OMP 可能已有部分安装状态，地址配置未修改。\n' + str(error))
        installed = plugin(command, path)
        if installed is None: fail('远端尚未安装 Codexpress 扩展，请先安装；地址配置未修改。')
        config['baseUrl'] = payload['base_url']
        written = (json.dumps(config, ensure_ascii=False, indent=2) + '\n').encode()
        atomic(fd, 'codexpress.json', written, before)
        try:
            run_cli(command, path, ['login', 'codexpress'], 40, credentials=payload)
            # Login belongs to OMP; never inspect or alter its credential database.
            final_raw, final = profile(fd)
            if final.get('baseUrl') != payload['base_url']:
                fail('OMP 登录期间地址配置已变化。')
            preserved = dict(config, **final)
            if preserved != final:
                atomic(fd, 'codexpress.json', (json.dumps(preserved, ensure_ascii=False, indent=2) + '\n').encode(), final_raw)
        except Exception as error:
            restored = False
            try:
                current_raw, current_config = profile(fd)
                if current_raw != written and current_config != config: fail('配置已被其他写入更改。')
                atomic(fd, 'codexpress.json', before, current_raw); restored = True
            except Exception: pass
            detail = str(error) if isinstance(error, AgentError) else '请检查远端配置文件权限和磁盘状态。'
            fail('OMP 原生登录失败；' + ('已恢复原地址配置。' if restored else '配置已被其他写入更改，已保留。') + '扩展及原生登录状态可能已改变。\n' + detail)
        result = extension_status(path, command, version(command), installed)
        result['connected'] = True
        return result
    finally:
        if lock is not None: os.close(lock)
        if meta is not None: os.close(meta)
        os.close(fd)

def discovery():
    home = os.path.expanduser('~')
    root = os.environ.get('PI_CONFIG_DIR') or '.omp'
    root = os.path.normpath(home + '/' + root)
    profile = os.environ.get('OMP_PROFILE', os.environ.get('PI_PROFILE', '')).strip()
    preferred = os.path.join(home, '.omp', 'agent')
    if profile and profile != 'default':
        if re.fullmatch(r'[a-z0-9][a-z0-9._-]{0,63}', profile) and not profile.endswith('.') and not re.match(r'(?i)^(CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])(?:\..*)?$', profile):
            preferred = os.path.join(root, 'profiles', profile, 'agent')
    else:
        override = os.environ.get('PI_CODING_AGENT_DIR', '')
        preferred = os.path.abspath(override) if override and not any(ord(c) < 32 or ord(c) == 127 for c in override) else os.path.join(root, 'agent')
    # XDG moves OMP data/state/cache, not the agent configuration directory.
    candidates = [selected(os.path.abspath(preferred))]
    parts = os.environ.get('SSH_CONNECTION', '').split()
    command = executable()
    return dict(candidates=candidates, client_ip=parts[0] if len(parts) == 4 else '', remote_host=socket.gethostname(),
                omp_executable=command, omp_version=version(command))

def main(payload):
    action = payload.get('action')
    if action == 'discover': return discovery()
    if action == 'probe':
        probe(payload)
        return dict(connected=True)
    path = selected(payload.get('agent_dir'))
    if action == 'status':
        command = executable()
        return extension_status(path, command, version(command), plugin(command, path) if command else None)
    if action in ('install', 'configure'): return connect_extension(payload, path, action == 'install')
    if action == 'read':
        fd = directory(path)
        try: return state(fd, path)
        finally:
            if fd is not None: os.close(fd)
    if action != 'restore': fail('未知 OMP 远端操作。')
    files, snapshot = bundle(payload)
    fd = directory(path)
    if fd is None: fail('远端 OMP 目录不存在，无法还原。')
    meta = lock = None
    try:
        meta = child_directory(fd, '.codexpress', True); os.fchmod(meta, 0o700)
        lock = os.open('remote.lock', os.O_WRONLY | os.O_NONBLOCK | os.O_CREAT | getattr(os, 'O_NOFOLLOW', 0), 0o600, dir_fd=meta)
        if not stat.S_ISREG(os.fstat(lock).st_mode) or os.fstat(lock).st_nlink != 1: fail('OMP 锁文件无效。')
        os.fchmod(lock, 0o600); fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        current = state(fd, path)
        if current['digest'] != payload.get('expected_digest'): fail('远端 OMP 配置已变化，请重新预览。')
        if current['agent_db_exists'] and not any(current['files'][n] is not None for n in ('config.yml', 'config.yaml')):
            fail('检测到未迁移的 OMP agent.db，请先在远端用 OMP 完成 YAML 迁移。')
        backup_id = backup(meta, current)
        os.fsync(meta)
        if state(fd, path)['digest'] != current['digest']: fail('备份期间远端 OMP 配置已变化，请重新预览。')
        changed = []
        try:
            for name in NAMES:
                before = decoded(current['files'][name], LIMIT)
                if files[name] == before: continue
                changed.append((fd, name, before, files[name])); atomic(fd, name, files[name], before)
            before = decoded(current['snapshot'], SNAPSHOT_LIMIT)
            if snapshot != before:
                changed.append((meta, 'attachment.json', before, snapshot)); atomic(meta, 'attachment.json', snapshot, before)
            final = state(fd, path)
            if final['files'] != payload['files'] or final['snapshot'] != payload['snapshot'] or final['agent_db_exists'] != current['agent_db_exists']:
                fail('写入期间远端 OMP 配置已变化。')
        except Exception:
            for target, name, before, written in reversed(changed):
                try: atomic(target, name, before, written)
                except Exception: pass
            fail('OMP 应用失败；已撤回仍属于本次操作的变更，保留并发修改和操作前备份。')
        return dict(applied=True, agent_dir=path, backup_id=backup_id, digest=final['digest'])
    finally:
        if lock is not None: os.close(lock)
        if meta is not None: os.close(meta)
        os.close(fd)

try:
    raw = sys.stdin.buffer.read(12 * 1024 * 1024 + 1)
    if len(raw) > 12 * 1024 * 1024: fail('OMP 远端请求过大。')
    print(json.dumps(dict(ok=True, data=main(json.loads(raw))), ensure_ascii=False))
except AgentError as error:
    print(json.dumps(dict(ok=False, error=str(error)), ensure_ascii=False))
except Exception:
    print(json.dumps(dict(ok=False, error='OMP 远端操作失败，请检查目录、符号链接、权限、Python 3 和连接状态。'), ensure_ascii=False))
"""#
}
