"""Record safe StringFormatter results using actual source util.identity.

Run with the installed gallery-dl environment:
  python generate_formatter_identity.py [output.json]
No expressions, modules, filesystem metadata, network, or external commands run.
"""
import hashlib
import json
from pathlib import Path
import sys
from gallery_dl import dt, formatter, util, version

context = {
    'num': 7, 'negative': -3, 'fraction': 2.75, 'flag': True, 'false': False,
    'text': ' 12 ', 'decimal': '-2.5', 'bad': 'bad', 'empty': '', 'none': None,
    'word': 'Cat', 'words': ['b', 'a', 'b'], 'numbers': [3, 1, 2],
    'mapping': {'b': 2, 'a': 1}, 'mixed': [1, 'a'], 'unicode': '猫',
    'date': dt.datetime(2024, 1, 2, 3, 4, 5),
    'ties': [1, True, 1.0, 2],
    'large_numbers': [9007199254740993, 9007199254740992.0, 9007199254740992, 9007199254740994.0],
}
templates = []
for key in context.keys() | {'missing'}:
    templates += ['{' + key + '}', 'x{' + key + '}y']
for key in ['num', 'fraction', 'flag', 'false', 'text', 'decimal', 'bad', 'empty', 'none', 'missing', 'numbers', 'date']:
    for conv in ['i', 'f', 's', 'S', 'r']:
        templates += ['{' + key + '!' + conv + '}', 'x{' + key + '!' + conv + '}y']
for key in ['word', 'num', 'none', 'missing']:
    for conv in ['l', 'u', 't']:
        templates.append('{' + key + '!' + conv + '}')
for key in ['words', 'numbers', 'mapping', 'unicode', 'none']:
    templates.append('{' + key + '!j}')
for key in ['num', 'fraction', 'flag', 'text', 'word', 'numbers', 'none', 'missing']:
    for op in ['A+3/', 'A-2/', 'A*2/']:
        templates += ['{' + key + ':' + op + '}', 'x{' + key + ':' + op + '}y']
for key in ['words', 'numbers', 'word', 'mapping', 'mixed', 'none', 'missing', 'ties', 'large_numbers']:
    for op in ['S/', 'Sr/', 'J,/', 'L3/long/']:
        templates.append('{' + key + ':' + op + '}')
for key in ['word', 'empty', 'none', 'num', 'words']:
    for op in ['?before/after/', 'Ra/x/', 'R/x/', 'Ci/', 'Cf/', 'Cs/']:
        templates.append('{' + key + ':' + op + '}')
templates += [
    '{num:03}', '{fraction:.1f}', '{word:>8}', '{none:03}', '{missing:03}',
    '{num:A+3/04}', '{words:S/J-/}', '{num:A*2/Cf/}', '{text!i:A+1/}',
    '{none!s:?before/after/}', '{false:S/}', '{numbers:L5/long/}',
    '{date!j}', 'x{date!j}y', '{date}', '{date:%Y-%m-%d}', '{date:O+02:00/}', '{date:O-01:30/}',
    '{date!T}', '{num!d}', '{text!d}', '{fraction!d}', '{negative!d}',
    '{bad!d}', '{none!d}', '{date!D}', "{'2024-01-02T03:04:05Z'!D}",
    '{date:O+02:00/%H:%M}', '{num!i:03}', '{{escaped}}', '',
]
# Stable matrix order makes regeneration and code review straightforward.
templates = list(dict.fromkeys(sorted(templates)))
def encode(value):
    if isinstance(value, dt.datetime):
        return {'$datetime': str(value)}
    return value
cases = []
for text in templates:
    case = {'template': text}
    try:
        value = formatter.parse(text, None, util.identity).format_map(context)
        case.update(type=type(value).__name__, value=encode(value))
    except Exception as exc:
        case['error'] = type(exc).__name__
    cases.append(case)
path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).with_name('formatter_identity.json')
path.write_text(json.dumps({
    'version': version.__version__,
    'source': 'Actual gallery_dl.formatter.parse(template, None, util.identity).format_map(context); only bounded safe StringFormatter conversions/specifiers.',
    'source_sha256': hashlib.sha256(Path(formatter.__file__).read_bytes()).hexdigest(),
    'generator': Path(__file__).name,
    'context': {key: encode(value) for key, value in context.items()},
    'cases': cases,
}, ensure_ascii=False, indent=2) + '\n')
print(len(cases))
