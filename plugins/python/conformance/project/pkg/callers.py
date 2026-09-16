"""pkg.callers - receiver calls whose target only a type checker knows.

Every call in this file is spelled ``<something>.method()`` and none of them
produces a structural edge: ``src/extractor/bodies.rs``'s Decision 7 records
one open site and no edge whenever the receiver is not the enclosing method's
own first parameter. What separates the three is what the receiver *is*, which
is the whole of what the semantic tier contributes:

- a local whose type comes from its initializer (``through_a_variable``);
- a parameter annotated with a base class (``through_a_base_annotation``);
- a local of a *subclass*, whose override is the honest answer rather than the
  base's declaration (``through_a_subclass``).

``pkg/mod.py``'s ``on_an_unknown_receiver`` is the fourth shape and stays in
that file: an unannotated parameter, which no tier resolves - see
``plugins/python/README.md`` on what ``resolved: true`` does not cover.
"""

from pkg.base import Base
from pkg.mod import Greeter
from pkg.sub.deep import Deep


def through_a_variable(name: str) -> str:
    """A receiver call on a local whose type is inferred from its initializer."""
    greeter = Greeter(name)
    return greeter.render()


def through_a_base_annotation(obj: Base) -> str:
    """A receiver call on a parameter annotated with the base class."""
    return obj.describe()


def through_a_subclass() -> str:
    """The same call on a subclass: the override, never the base's declaration."""
    deep = Deep()
    return deep.describe()
