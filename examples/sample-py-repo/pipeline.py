"""Tiny intra-module pipeline. Exists to give ASD's call-graph extractor
something resolvable inside a single file: free functions and methods on a
class, all calling each other by name.
"""


def normalize(name: str) -> str:
    """Trim and lower-case ``name``. Pure."""
    return name.strip().lower()


def format_label(name: str) -> str:
    """Compose a label by reusing :func:`normalize`. Emits a debug log so
    callers pick up a ``log`` effect transitively — this is the function
    that lets ASD demo transitive effect propagation on the sample repo."""
    label = f"user:{normalize(name)}"
    print(f"[pipeline] label={label}")
    return label


class Pipeline:
    """Two-step pipeline that composes :func:`format_label` via methods."""

    def __init__(self, prefix: str) -> None:
        self.prefix = normalize(prefix)

    def label_for(self, name: str) -> str:
        """Apply the module-level helper, then prefix the result."""
        base = format_label(name)
        return self._with_prefix(base)

    def _with_prefix(self, base: str) -> str:
        """Internal helper called via ``self`` — exercises self-call resolution."""
        return f"{self.prefix}/{base}"
