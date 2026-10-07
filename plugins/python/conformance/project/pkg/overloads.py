"""pkg.overloads - a ``typing.overload`` set, and a call into it from here.

``coerce`` is one function written three times: two ``@overload`` stubs and
the implementation. The extractor emits one node (the first stub's range and
signature) carrying all three as ``declarations``, ordinals 0, 1 and 2 in
source order, ``hasBody`` only on the implementation. Which stub a call binds
is a type checker's answer, never a structural one; see
``docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md``.

``Codec.encode`` is the same shape as a method, reached through a receiver
from ``pkg/overload_callers.py``.
"""

from typing import Union, overload


@overload
def coerce(value: int) -> int: ...
@overload
def coerce(value: str) -> str: ...
def coerce(value: Union[int, str]) -> Union[int, str]:
    """The implementation: what runs, and what no call binds."""
    return value


class Codec:
    """An overload set as a method."""

    @overload
    def encode(self, value: int) -> int: ...
    @overload
    def encode(self, value: str) -> str: ...
    def encode(self, value):
        return value


def coerce_here() -> int:
    """A call into the set from its own file: the structural edge is direct."""
    return coerce(1)
