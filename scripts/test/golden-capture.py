#!/usr/bin/env python3
"""Capture the golden CLI corpus from a reference wally binary.

    golden-capture.py --binary <wally> --out tests/golden [--cases tests/golden/cases.json]

The committed corpus was captured from the C++ build of d1e9c0b; see
tests/golden/README.md. tests/cli_golden.rs must use the same environment.

Every case runs in a fresh, isolated home (HOME, RUNANYWHERE_HOME,
WALLY_PROFILE_DIR, XDG_*) that is discarded once the case finishes, with
stdin closed, no colour, a dead local base URL so no telemetry leaves the
machine, and a PATH that cannot find any coding tool. Only invocations that
fail to parse or touch nothing outside that throwaway home are listed:
nothing here updates, uninstalls, serves, downloads or launches a tool
outside it.

Output: <out>/cases.json (the case list) and <out>/expected/<name>.{stdout,
stderr,code}. The temporary home path is replaced by the literal `$HOME` so the
files are stable across runs, and the product version by `<VERSION>` so a
release bump leaves them valid; tests/cli_golden.rs applies the same rewrites.
"""
import argparse, json, os, re, shutil, subprocess, sys, tempfile

PASSTHROUGH = {'opencode', 'claude-code', 'claude-desktop', 'hermes', 'openclaw', 'deepseek'}
SECTIONS = {'Commands', 'Chat', 'Models', 'Coding tools', 'Account', 'Wally'}
# Leaf commands whose invalid invocations are safe to run (parse fails first).
SAFE_LEAVES = ['run', 'llm generate', 'llm stream', 'llm tool-call', 'serve',
               'models list', 'models show', 'models pull', 'models rm', 'models default',
               'models load', 'models unload', 'models register', 'models state',
               'account login', 'account logout', 'account whoami', 'account usage',
               'about', 'info', 'version', 'backends', 'bench', 'telemetry emit', 'telemetry blast',
               'update', 'uninstall']


def env_for(home):
    return {
        'PATH': '/usr/bin:/bin', 'HOME': home, 'USERPROFILE': home,
        'RUNANYWHERE_HOME': os.path.join(home, 'ra'),
        'WALLY_PROFILE_DIR': os.path.join(home, 'profile'),
        'XDG_STATE_HOME': os.path.join(home, 'state'),
        'XDG_CONFIG_HOME': os.path.join(home, 'config'),
        'XDG_DATA_HOME': os.path.join(home, 'data'),
        'RUNANYWHERE_BASE_URL': 'http://127.0.0.1:9',
        'TERM': 'dumb', 'LANG': 'C', 'LC_ALL': 'C',
    }


def run_case(binary, args, timeout=60):
    home = tempfile.mkdtemp(prefix='wally-golden-')
    try:
        argv = [a.replace('$HOME', home) for a in args]
        try:
            p = subprocess.run([binary] + argv, env=env_for(home), capture_output=True,
                               stdin=subprocess.DEVNULL, timeout=timeout)
        except subprocess.TimeoutExpired:
            return None, '', ''
        def norm(b):
            text = b.decode('utf-8', 'replace').replace(os.path.realpath(home), '$HOME').replace(home, '$HOME')
            # The product version moves with every release; tests/cli_golden.rs
            # writes the same placeholder.
            text = re.sub(r'\bwally \d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?(?=[ \n])', 'wally <VERSION>', text)
            return re.sub(r'"wally":"\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?"', '"wally":"<VERSION>"', text)
        return p.returncode, norm(p.stdout), norm(p.stderr)
    finally:
        shutil.rmtree(home, ignore_errors=True)


def subcommands(text):
    names, cur = [], None
    for line in text.splitlines():
        m = re.match(r'^([A-Z][A-Za-z ]*):$', line)
        if m:
            cur = m.group(1)
            continue
        if cur in SECTIONS:
            m = re.match(r'^  ([a-z][a-z0-9-]*(?: [a-z][a-z0-9-]*)?)(?:\s{2,}|$)', line)
            if m:
                names.append(m.group(1).split(' '))
    return names


OPT_RE = re.compile(r'^  (?:(-\w), )?\s*(--[\w-]+)((?:, --[\w-]+)*)(?: ([A-Z]+|\{[^}]*\}))?( \.\.\.)?( REQUIRED)?\s*(?:\s{2}|$)')
POS_RE = re.compile(r'^Usage: wally ([a-z -]+?) \[OPTIONS\](.*)$')


def parse_help(text):
    opts, positionals = [], []
    for line in text.splitlines():
        m = POS_RE.match(line)
        if m:
            for tok in m.group(2).split():
                if tok in ('COMMAND', '[COMMAND]'):
                    continue
                positionals.append((tok.strip('[]'), not tok.startswith('[')))
        m = OPT_RE.match(line)
        if m and m.group(2) not in ('--help', '--version'):
            opts.append({'long': m.group(2), 'short': m.group(1), 'type': m.group(4) or '',
                         'multi': bool(m.group(5)), 'required': bool(m.group(6))})
    return opts, positionals


def slug(args):
    return '__'.join(re.sub(r'[^A-Za-z0-9._=-]', '', a.replace('$HOME', 'HOME')) for a in args)


def build_cases(binary):
    cases = []
    add = lambda name, args: cases.append({'name': name, 'args': args})
    # Help for every reachable command path, plus hidden ones by name.
    seen, queue = set(), [[]] + [p.split(' ') for p in [
        'run', 'llm', 'llm generate', 'llm stream', 'llm tool-call', 'info', 'version', 'help', 'bench',
        'backends', 'telemetry', 'telemetry emit', 'telemetry blast', 'models load', 'models unload',
        'models register', 'models state', 'models ls', 'models get', 'models download', 'models remove',
        'models delete', 'models catalog', 'account usage']]
    helps = {}
    while queue:
        path = queue.pop(0)
        key = ' '.join(path)
        if key in seen:
            continue
        seen.add(key)
        args = (['help'] + path) if path and path[0] in PASSTHROUGH else path + ['--help']
        if path and path[0] in PASSTHROUGH and len(path) > 1:
            continue
        name = 'help__' + (key.replace(' ', '__') or 'ROOT')
        add(name, args)
        rc, out, _ = run_case(binary, args, timeout=30)
        helps[key] = out
        if rc != 0:
            continue
        for parts in subcommands(out):
            cand = parts if not path else path + parts
            if len(cand) <= 3:
                queue.append(cand)
    # Alternate help spellings.
    for args in (['-h'], ['help'], ['help', 'models'], ['help', 'nope'], ['help', 'models', 'pull'],
                 ['models', '-h'], ['--no-color', '--help'], ['models', 'pull', '-h'], ['--help', 'models'],
                 ['help', 'opencode'], ['help', 'claude-code']):
        add('alt_help__' + slug(args), args)
    # Version and bare invocations.
    for args in ([], ['--version'], ['-V'], ['version'], ['version', '--json'], ['--json', 'version'],
                 ['-V', 'models'], ['--version', '--json']):
        add('version__' + (slug(args) or 'bare'), args)
    # Unknown / unregistered commands.
    for word in ('stt', 'tts', 'vad', 'vlm', 'embed', 'rerank', 'image', 'diarize', 'segment', 'voice',
                 'rag', 'lora', 'auth', 'chat', 'list', 'ls', 'pull', 'rm', 'show', 'foo'):
        add('unknown__' + word, [word])
    for args in (['models', 'foo'], ['llm', 'foo'], ['account', 'foo'], ['telemetry', 'foo'], ['models'],
                 ['llm'], ['account'], ['telemetry'], ['models', 'list', '-u'], ['-u', '-v'], ['-U', '--json'],
                 ['--bogus'], ['-z'], ['--home'], ['--json', '--bogus', 'models', 'list']):
        add('parse__' + slug(args), args)
    # Per-leaf invalid invocations derived from each leaf's help.
    for leaf in SAFE_LEAVES:
        text = helps.get(leaf)
        if text is None:
            continue
        opts, positionals = parse_help(text)
        path = leaf.split(' ')
        req = ['x' for (_, required) in positionals if required]
        base = path + req
        stem = leaf.replace(' ', '__')
        add(f'leaf__{stem}__bogus_flag', base + ['--bogus-flag'])
        add(f'leaf__{stem}__extra_positional', base + ['one', 'two', 'three', 'four'])
        if req:
            add(f'leaf__{stem}__missing_positional', path)
        for o in opts:
            t, lng = o['type'], o['long']
            tag = lng.strip('-')
            if t in ('INT', 'UINT', 'FLOAT', 'POSITIVE', 'NONNEGATIVE') or t.startswith('INT:') :
                add(f'leaf__{stem}__{tag}__not_a_number', base + [lng, 'abc'])
                add(f'leaf__{stem}__{tag}__missing_value', base + [lng])
            if t in ('INT', 'UINT', 'POSITIVE'):
                add(f'leaf__{stem}__{tag}__fraction', base + [lng, '1.5'])
            if t in ('UINT', 'POSITIVE'):
                add(f'leaf__{stem}__{tag}__negative', base + [f'{lng}=-1'])
            if t == 'POSITIVE':
                add(f'leaf__{stem}__{tag}__zero', base + [lng, '0'])
            if t.startswith('{'):
                add(f'leaf__{stem}__{tag}__not_member', base + [lng, 'zzz'])
            if t == 'FILE':
                add(f'leaf__{stem}__{tag}__missing_file', base + [lng, '/nonexistent/wally-golden-file'])
            if t == 'TEXT':
                add(f'leaf__{stem}__{tag}__missing_value', base + [lng])
            if o['required']:
                add(f'leaf__{stem}__{tag}__required_missing', path + req)
    # Local commands that succeed or fail on local state only; most only read
    # it, but the `models default --clear`/`some-id` cases write preferences
    # into the throwaway home set up by run_case, and nowhere else.
    for args in (['models', 'list'], ['models', 'list', '--json'], ['--json', 'models', 'list'],
                 ['models', 'ls'], ['models', 'list', '--all'], ['models', 'list', '--all', '--json'],
                 ['models', 'show', 'qwen3-0.6b'], ['models', 'show', 'qwen3-0.6b', '--json'],
                 ['models', 'show', 'no-such-model'], ['models', 'rm', 'no-such-model'],
                 ['models', 'default'], ['models', 'default', '--json'],
                 ['models', 'default', 'some-id', '--json'], ['models', 'default', '--clear'],
                 ['models', 'default', '--clear', '--json'],
                 ['account', 'whoami'], ['account', 'whoami', '--json'], ['--json', 'account', 'whoami'],
                 ['account', 'usage'], ['account', 'usage', '--json'], ['account', 'logout'],
                 ['backends'], ['backends', '--json'], ['-q', 'models', 'list'],
                 ['--home', '$HOME/elsewhere', 'models', 'list', '--json'],
                 ['models', 'list', '--home', '$HOME/elsewhere'], ['llm', 'generate', 'no-such-model', 'hi'],
                 ['run', 'no-such-model', 'hi'], ['run', 'no-such-model', 'hi', '--json'],
                 ['bench', 'no-such-model']):
        add('local__' + slug(args), args)
    return cases


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--binary', required=True)
    ap.add_argument('--out', required=True)
    ap.add_argument('--cases', help='reuse this case list instead of discovering one')
    a = ap.parse_args()
    cases = json.load(open(a.cases)) if a.cases else build_cases(a.binary)
    names = [c['name'] for c in cases]
    dupes = {n for n in names if names.count(n) > 1}
    if dupes:
        sys.exit(f'duplicate case names: {sorted(dupes)}')
    exp = os.path.join(a.out, 'expected')
    os.makedirs(exp, exist_ok=True)
    json.dump(cases, open(os.path.join(a.out, 'cases.json'), 'w'), indent=1)
    kept = []
    for c in cases:
        rc, out, err = run_case(a.binary, c['args'], timeout=30)
        if rc is None:
            print(f"dropped (did not exit in 30 s, so it is not a safe parse-only case): {c['name']}")
            continue
        kept.append(c)
        for ext, data in (('stdout', out), ('stderr', err), ('code', f'{rc}\n')):
            with open(os.path.join(exp, f"{c['name']}.{ext}"), 'w', newline='') as f:
                f.write(data)
    json.dump(kept, open(os.path.join(a.out, 'cases.json'), 'w'), indent=1)
    print(f'{len(kept)} cases captured into {a.out}')


if __name__ == '__main__':
    main()
