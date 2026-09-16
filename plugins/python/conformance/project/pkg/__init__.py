"""The fixture's top-level package - pkg.

Its ``__all__`` is the acceptance case for the re-export chain: nothing is
declared in this file, so ``from pkg import Greeter`` can only reach the class
in ``pkg/mod.py`` by walking this package's re-exports.
"""

from .mod import Greeter, greet
from .helpers import assist as helper

__all__ = ["Greeter", "greet", "helper"]
