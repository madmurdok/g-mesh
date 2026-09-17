"""pkg.plugins is a PEP 420 namespace package - no __init__.py here.

It is also where the star import lives: ``from pkg.helpers import *``
republishes whatever that module exports, which is the ``*``-at-both-ends
re-export shape, and ``import os.path`` is the external one - a dotted name
this project does not contain.
"""

import os.path
from pkg.helpers import *
from pkg.mod import greet


def load(name: str) -> str:
    """Call across the namespace package, into pkg.mod."""
    return greet(name)


def path_of(name: str) -> str:
    """An external import: ``os.path`` is not one of this project's own."""
    return os.path.join("plugins", name)
