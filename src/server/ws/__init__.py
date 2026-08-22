__all__ = ["send_packet"]


def __getattr__(name: str):
    # Keep importing lightweight submodules (notably ws.sender) from pulling the
    # application runtime back into core.broadcaster during module initialization.
    if name == "send_packet":
        from .io import send_packet

        return send_packet
    raise AttributeError(name)
