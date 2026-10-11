import ast
import builtins as python_builtins
import sys
import json

# This program parses and syntax-compiles input. It never evals, imports or executes supplied code.
try:
    mode = sys.argv[1] if len(sys.argv) > 1 else 'legacy'
    generic = mode in {'code', 'pre', 'post'}
    tree = ast.parse(sys.stdin.read())
    parents = {child: node for node in ast.walk(tree) for child in ast.iter_child_nodes(node)}
    modules = {'json', 're', 'datetime'}
    builtins = {'str', 'int', 'float', 'bool', 'len', 'min', 'max', 'round', 'sum', 'abs', 'isinstance', 'dict', 'list', 'set', 'sorted', 'enumerate', 'range', 'zip', 'any', 'all', 'ValueError', 'TypeError', 'RuntimeError'}
    builtin_names = set(dir(python_builtins))
    forbidden = {'FunctionDef', 'AsyncFunctionDef', 'ClassDef', 'Lambda', 'With', 'AsyncWith', 'Await', 'Yield', 'YieldFrom', 'Global', 'Nonlocal', 'Delete', 'While'}
    for node in ast.walk(tree):
        assert type(node).__name__ not in forbidden, 'unsupported Python construct'
        if isinstance(node, ast.Import):
            assert all(a.name in modules and a.asname is None for a in node.names), 'only unaliased json, re and datetime imports allowed'
        if isinstance(node, ast.ImportFrom):
            assert False
        # Exception and pattern captures store identifiers as strings, not Name nodes.
        binding = None
        if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Store):
            binding = node.id
        elif type(node).__name__ in {'ExceptHandler', 'MatchAs', 'MatchStar'}:
            binding = node.name
        elif type(node).__name__ == 'MatchMapping':
            binding = node.rest
        assert binding not in builtin_names, 'builtin rebinding is forbidden'
        if isinstance(node, ast.Name):
            assert not node.id.startswith('_')
            assert node.id not in {'open', 'eval', 'exec', 'compile', 'getattr', 'setattr', 'delattr', 'globals', 'locals', 'input', 'help', 'breakpoint'}
            if node.id == 'config':
                p = parents[node]
                assert mode == 'pre' and isinstance(p, ast.Subscript) and p.value is node and isinstance(p.slice, ast.Constant) and p.slice.value == 'body', 'only config body may be accessed by a pre-hook; URL/method/headers are pinned'
            if node.id == 'result':
                assert mode == 'post', 'result is available only in post-hooks'
            if node.id == 'vars':
                p = parents[node]
                if isinstance(p, ast.Attribute):
                    call = parents.get(p)
                    assert p.attr == 'get' and isinstance(call, ast.Call) and call.func is p, 'vars.get must be the direct callee of a checked call'
                else:
                    assert isinstance(p, ast.Subscript) and p.value is node
        if isinstance(node, ast.Attribute):
            assert not node.attr.startswith('_')
            assert node.attr not in {'format', 'format_map', 'load', 'dump', 'read', 'write'}
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            assert (generic or 'builder_' not in node.value) and '__' not in node.value, 'reserved compiler state or dunder string'
            assert not node.value.startswith(('system.', 'extractor.', 'call.')), 'runtime transcript/call state is reserved (including mutable aliases)'
        if isinstance(node, ast.Subscript) and isinstance(node.value, ast.Name) and node.value.id == 'vars':
            assert isinstance(node.slice, ast.Constant) and isinstance(node.slice.value, str)
            assert generic or node.slice.value != 'customer_id'
            if generic:
                assert node.slice.value.lower() not in {'authorization', 'api_key', 'apikey', 'access_token', 'refresh_token', 'password', 'secret', 'x-team-key'}, 'secret variable access forbidden'
        if isinstance(node, ast.Call):
            if isinstance(node.func, ast.Attribute) and isinstance(node.func.value, ast.Name) and node.func.value.id == 'vars':
                assert node.func.attr == 'get' and node.args and isinstance(node.args[0], ast.Constant) and isinstance(node.args[0].value, str), 'vars.get requires a literal key'
                if generic:
                    assert node.args[0].value.lower() not in {'authorization', 'api_key', 'apikey', 'access_token', 'refresh_token', 'password', 'secret', 'x-team-key'}, 'secret variable access forbidden'
            if isinstance(node.func, ast.Name):
                assert node.func.id in builtins, 'function call not in builtin allowlist'
            else:
                assert isinstance(node.func, ast.Attribute)
                assert node.func.attr in {'get', 'items', 'values', 'keys', 'append', 'extend', 'strip', 'lower', 'upper', 'replace', 'split', 'join', 'isdigit', 'startswith', 'endswith', 'index', 'ljust', 'rjust', 'zfill', 'lstrip', 'rstrip', 'casefold', 'splitlines', 'utcoffset', 'loads', 'dumps', 'match', 'fullmatch', 'search', 'sub', 'findall', 'group', 'date', 'time', 'datetime', 'combine', 'timedelta', 'fromisoformat', 'strptime', 'isoformat', 'weekday', 'strftime', 'total_seconds'} | ({'pop'} if generic else set()), 'method not in safe allowlist: ' + node.func.attr
    # Compile for semantic syntax checks (e.g. return/break outside their scope).
    # The resulting code object is NEVER executed on this host.
    compile(tree, '<embedded-validation-only>', 'exec')
    print(json.dumps(sorted({node.slice.value for node in ast.walk(tree) if isinstance(node, ast.Subscript) and isinstance(node.value, ast.Name) and node.value.id == 'vars' and isinstance(node.ctx, ast.Store)})))
except Exception as exc:
    current = locals().get('node')
    print(json.dumps({'line': getattr(current, 'lineno', 0), 'node': type(current).__name__, 'error': str(exc) or 'AST safety restriction'}))
    sys.exit(1)
