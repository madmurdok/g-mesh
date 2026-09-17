"""An ordinary module - pkg.mod."""

import pkg.helpers
import pkg.base as base_module
from pkg.base import Base
from . import helpers
from .helpers import assist

GREETING: str = "hello"


def greet(name: str) -> str:
    """Return a greeting for name."""
    return decorate(GREETING, name)


def decorate(prefix: str, name: str) -> str:
    """Join a prefix and a name."""
    return prefix + ", " + name


def call_through_module(what: str) -> str:
    """A module-qualified call: the qualifier is an imported module."""
    return helpers.assist(what)


def call_through_imported_name(what: str) -> str:
    """A bare call to a name imported by ``from ... import``."""
    return assist(what)


def call_through_a_class(name: str) -> str:
    """A class-qualified call: the qualifier is an imported class."""
    return Base.describe(Greeter(name))


def on_an_unknown_receiver(obj) -> str:
    """The receiver gap: nothing is known about ``obj``, so no edge at all."""
    return obj.describe()


def module_alias_is_bound(name: str) -> str:
    """``import pkg.base as base_module`` binds an alias to a container."""
    return base_module.Base().describe()


class Greeter(Base):
    """A Greeter, inheriting from a base declared in another module."""

    def __init__(self, name: str) -> None:
        self.name = name

    def shout(self) -> str:
        """Call a sibling method through this method's own instance."""
        return self.render().upper()

    def render(self) -> str:
        """Use a module-level function from inside a method."""
        return greet(self.name)


def outer() -> str:
    """A nested definition - ``outer.inner`` - called from its parent."""

    def inner() -> str:
        return decorate("nested", "call")

    return inner()
