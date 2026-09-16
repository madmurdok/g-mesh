"""pkg.gaps - the plugin's documented structural gaps, beside code that has them.

Everything in this file is *correct* Python that this tier deliberately does
not resolve. It is here so that the gap list in ``plugins/python/README.md``
can be read against real source rather than as a list of claims, and so that a
future semantic tier has a fixture to measure itself against. Nothing here is
named in ``conformance/expect.toml``: an expectation over a gap would be an
assertion that it stays a gap forever, and these are exactly the things a
pyright tier is expected to close.
"""

import functools

from .base import Base

REGISTRY = {}


def gap_1_dynamic_attributes(target):
    """setattr and __getattr__ create names that exist only at runtime.

    ``target.installed`` has no declaration anywhere for a reference edge to
    land on, so nothing points at it and nothing ever will from this tier.
    """
    setattr(target, "installed", True)
    return REGISTRY["installed"]


def gap_2_monkey_patching():
    """Rebinding a name after import is invisible here.

    The index shows ``Base.describe`` as written in ``pkg/base.py``; after this
    runs, every caller reaches ``replacement`` instead, and no edge says so.
    """

    def replacement(self):
        return "patched"

    Base.describe = replacement


try:
    # Gap 3: a conditional import. BOTH branches are indexed and no predicate
    # is evaluated, so `get_dependencies` reports this file as depending on
    # `pkg.helpers` *and* `pkg.plugins.loader` even though only one of them is
    # ever bound at runtime.
    from .helpers import SEPARATOR as JOINER
except ImportError:  # pragma: no cover
    from .plugins.loader import path_of as JOINER


def gap_4_star_import_names(what):
    """A name that arrives through ``from … import *`` resolves to nothing.

    ``pkg/plugins/loader.py`` star-imports ``pkg.helpers``. Which names that
    actually binds depends on the other module's own ``__all__``, which may be
    computed at import time - so a bare call to one of them, there or here, has
    no address this tier can write down.
    """
    return undefined_until_star_import(what)


def gap_5_decorator_replaces_the_function(fn):
    """A decorator may return something other than what it decorated.

    ``wrapped`` is what callers actually reach; the index shows the
    ``@memoized`` declaration as written, with the decorator recorded in its
    signature and as a reference, and nothing that says the call goes through
    the wrapper.
    """

    @functools.wraps(fn)
    def wrapped(*args, **kwargs):
        return fn(*args, **kwargs)

    return wrapped


@gap_5_decorator_replaces_the_function
def memoized(value):
    """Decorated, and therefore not what a caller of ``memoized`` reaches."""
    return value
