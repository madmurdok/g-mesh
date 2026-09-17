"""pkg.consumer - reaches two declarations through the package's own __all__.

Neither name is declared in ``pkg/__init__.py``: ``Greeter`` lives in
``pkg/mod.py`` and ``assist`` in ``pkg/helpers.py`` under a different name.
The only path from here to either is core's re-export walk over the
``reexport`` nodes that file's ``__all__`` produces.
"""

from pkg import Greeter, helper


def build(name: str) -> str:
    """Reach pkg.mod.Greeter and pkg.helpers.assist through pkg's __all__."""
    return helper(Greeter(name).name)
