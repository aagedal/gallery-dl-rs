# Expression reference cases

`python.json` records Boolean results from a local CPython 3.15.0 interpreter
(Unicode 17.0.0). The expressions are synthetic and contain no account data.
`generate.py` regenerates this fixture from the repository root. Python is used
only to maintain the reference vectors; Rust tests and runtime do not invoke it.
The `contains` helper follows gallery-dl's public helper contract: strings split
on the separator by default, and a sequence of needles matches any element.

The reference covers precedence, short circuiting, conditional expressions,
exact integer/float comparisons, floor division/modulo, rounding, Unicode string
operations, indexing, distinct list/tuple semantics, dictionary lookup, common
builtins, and error-to-false behavior, and seeded numeric cases around integer-ratio and
decimal-rounding boundaries. It does not establish full Python syntax,
helper, datetime, regex, or arbitrary extractor-metadata compatibility. Unsupported
syntax/calls are rejected explicitly by the native parser.
