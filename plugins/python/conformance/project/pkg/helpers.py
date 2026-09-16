"""pkg.helpers - reached through a module-qualified call and a star import."""

SEPARATOR = ", "


def assist(what: str) -> str:
    """Do the small thing pkg.mod delegates."""
    return SEPARATOR.join(["assisted", what])
