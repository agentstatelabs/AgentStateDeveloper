"""Pure greeting helpers. No I/O, no logging, no side effects."""


def hello(name: str) -> str:
    """Return a simple hello string for ``name``."""
    return f"Hello, {name}!"


def format_welcome(name: str, locale: str = "en") -> str:
    """Return a localized welcome string. Deterministic, pure."""
    templates = {
        "en": "Welcome, {name}.",
        "es": "Bienvenido, {name}.",
        "fr": "Bienvenue, {name}.",
    }
    template = templates.get(locale, templates["en"])
    return template.format(name=name)


class Greeter:
    """Tiny greeter that composes a salutation with a name."""

    def __init__(self, salutation: str = "Hello") -> None:
        self.salutation = salutation

    def greet(self, name: str) -> str:
        """Return ``"<salutation>, <name>!"``."""
        return f"{self.salutation}, {name}!"
