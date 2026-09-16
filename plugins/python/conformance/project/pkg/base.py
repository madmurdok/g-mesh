"""pkg.base - the class pkg.sub.deep inherits from, two packages away."""


class Base:
    """A base class, imported and subclassed from two other modules."""

    def describe(self) -> str:
        """Name this object, for subclasses to extend."""
        return "base"
