"""pkg.base - the class pkg.sub.deep inherits from, two packages away."""


class Base:
    """A base class, imported and subclassed from two other modules."""

    def describe(self) -> str:
        """Name this object, for subclasses to extend."""
        return "base"


class Speaker:
    """A base class subclassed only through a star import - see pkg/dynamic.py.

    Nothing imports this by item, so no structural edge reaches it from its
    one subclass. It is here to be asked, from this end, who implements it.
    """

    def speak(self) -> str:
        """Say something, for subclasses to replace."""
        return "speak"
