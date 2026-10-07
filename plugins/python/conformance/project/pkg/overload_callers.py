"""pkg.overload_callers - calls into ``pkg/overloads.py``'s overload sets.

``calls_both_overloads`` calls ``coerce`` twice, once per stub. Structurally
that is one ``CALLS`` edge onto ``coerce``; with pyright it is two, one per
bound declaration - which is the whole difference the semantic tier makes
here, and what ``conformance/expect.toml``'s ``coerce`` entry counts.

Both calls cross a file boundary on purpose: the declarations pyright is asked
to hover are in a file other than the one being asked about.
"""

from pkg.overloads import Codec, coerce


def calls_both_overloads() -> str:
    """One call per ``@overload`` stub of ``coerce``."""
    number = coerce(1)
    text = coerce("s")
    return f"{number}{text}"


def through_a_receiver(codec: Codec) -> str:
    """A receiver call into a method overload set."""
    return codec.encode("s")
