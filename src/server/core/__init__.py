__all__ = ["Broadcaster"]


def __getattr__(name: str):
    if name == "Broadcaster":
        from .broadcaster import Broadcaster

        return Broadcaster
    raise AttributeError(name)
