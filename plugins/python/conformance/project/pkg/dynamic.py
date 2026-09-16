"""pkg.dynamic - a subclass whose base arrived through a star import.

``Speaker`` is neither declared here nor imported by item: it is bound by
``from pkg.base import *``, whose name set depends on the other module's own
``__all__`` and is therefore gap 4 of ``plugins/python/README.md``. The
extractor resolves a bare type name that is neither declared nor imported by
item to ``Bound::Nothing`` - no edge, and deliberately not even an open site,
since recording every unresolved bare name would make the open-site set mostly
builtins (``src/extractor/bodies.rs``, Decision 7).

So nothing structural says ``Megaphone`` extends ``Speaker``, and no question
the *open sites* generate could ever say it either. It is reachable only from
the other end, by asking the base class who implements it - which is what the
bridge's implementation sweep does for every node whose ``nativeKind`` the
manifest lists.
"""

from pkg.base import *


class Megaphone(Speaker):  # noqa: F405 - the point of this file
    """Extends a base this tier cannot see, through a star import."""

    def speak(self) -> str:
        """Override the star-imported base's own method."""
        return "MEGAPHONE"
