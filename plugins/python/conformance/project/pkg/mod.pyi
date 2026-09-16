"""A stub for pkg.mod.

Nothing in this file reaches the graph beyond its own ``File`` node:
``crate::project``'s Decision 6 and the extractor's Decision 8 between them
say a ``.pyi`` contributes no declaration, no self-announcement and no import.
The declarations below are deliberately the same names ``pkg/mod.py``
declares, which is exactly the case that would make ``from pkg import mod``
ambiguous if a stub announced itself.
"""

from pkg.base import Base

GREETING: str

def greet(name: str) -> str: ...
def decorate(prefix: str, name: str) -> str: ...

class Greeter(Base):
    def shout(self) -> str: ...
    def render(self) -> str: ...
