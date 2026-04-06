from importlib import reload

from fastapi import FastAPI

from server.app import runtime as _runtime_module

reload(_runtime_module)

from server.admin.routes import register_admin_routes  # noqa: E402
from server.app.lifecycle import lifespan  # noqa: E402
from server.app.routes import register_app_routes  # noqa: E402
from server.ws.routes import register_websocket_routes  # noqa: E402


app = FastAPI(lifespan=lifespan)
register_admin_routes(app)
register_websocket_routes(app)
register_app_routes(app)


if __name__ == "__main__":
    import uvicorn
    from server.app.uvicorn_websocket_protocol import TeamViewerWebSocketProtocol

    uvicorn.run(app, host="0.0.0.0", port=8765, ws=TeamViewerWebSocketProtocol, ws_per_message_deflate=True)
