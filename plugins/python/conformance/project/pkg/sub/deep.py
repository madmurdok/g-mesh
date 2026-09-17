"""pkg.sub.deep - exercises the parent chain two levels down.

Both of its imports are relative and both go **two levels up**: from
``pkg.sub.deep`` the one dot of ``.`` is ``pkg.sub``, so ``..`` is ``pkg``.
"""

from ..base import Base
from .. import helpers


class Deep(Base):
    """Inherits from a base reached by a relative import two levels up."""

    def describe(self) -> str:
        """Override the base's own method, through a relative import."""
        return helpers.assist("deep")
