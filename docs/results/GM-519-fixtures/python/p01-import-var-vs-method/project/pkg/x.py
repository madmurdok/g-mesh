def _make():
    return lambda: 1


handler = _make()


class Router:
    def handler(self):
        return 2
