"""Application lifecycle and HTTP routes. Run one worker per database."""

import asyncio
import logging
from contextlib import asynccontextmanager, suppress
from uuid import uuid4

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse, StreamingResponse

from .backend import ConnectionPool
from .errors import ProxyError
from .schemas import ChatRequest
from .service import ChatService
from .settings import settings as default_settings
from .storage import StateStore, build_backend
from .transport import collect, error_for, stream

log = logging.getLogger(__name__)


def create_app(settings=None, pool=None):
    settings = settings or default_settings

    # Thinking/search mode: patches the installed aiodeepseek client.
    # Idempotent. Applied here because
    # create_app is the single shared entry point for every launch path
    # (module-level `app`, `run()`, `__main__`, and tests).
    from .thinking import apply as apply_thinking

    # Always install: request-level reasoning parameters can enable thinking
    # even when the global THINKING_ENABLED setting is false.
    apply_thinking(True)

    @asynccontextmanager
    async def lifespan(app):
        nonlocal pool
        owned = pool is None
        store = None
        lock_file = None
        flush = None
        try:
            if owned:
                backend_kind = getattr(settings, "storage_backend", "sqlite")
                backend = build_backend(
                    backend_kind,
                    db_path=str(settings.db_path),
                    mongo_uri=getattr(settings, "mongodb_uri", ""),
                    mongo_db=getattr(settings, "mongodb_db", "deepseek_proxy"),
                )
                if backend_kind == "sqlite":
                    # Account locks are process-local: prevent accidental multi-worker
                    # use. Only meaningful for the on-disk SQLite backend; a remote
                    # backend relies on running a single instance (e.g. one Render
                    # service).
                    import fcntl

                    settings.db_path.parent.mkdir(parents=True, exist_ok=True)
                    lock_file = open(str(settings.db_path) + ".lock", "a")
                    try:
                        fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError as exc:
                        raise RuntimeError(
                            "Database already owned by a proxy process; run one worker"
                        ) from exc
                store = StateStore(str(settings.db_path), backend=backend)
                store.open()
                pool = ConnectionPool(
                    settings.accounts,
                    store,
                    settings.idle_timeout,
                    queue_limit=settings.queue_limit,
                    queue_timeout=settings.queue_timeout,
                    request_delay=settings.request_delay,
                )
            app.state.service = ChatService(pool, settings)

            async def flush_periodically():
                while True:
                    await asyncio.sleep(5)
                    try:
                        await asyncio.to_thread(store.snapshot)
                    except Exception:
                        log.exception("State snapshot failed")

            if store:
                flush = asyncio.create_task(flush_periodically())
            yield
        finally:
            if flush:
                flush.cancel()
                with suppress(asyncio.CancelledError):
                    await flush
            try:
                if owned and pool:
                    await pool.close()
            finally:
                if store:
                    await asyncio.to_thread(store.close)
                if lock_file:
                    lock_file.close()

    app = FastAPI(title="DeepSeek OpenAI Proxy", version="0.2.0", lifespan=lifespan)
    app.add_middleware(
        CORSMiddleware,
        allow_origin_regex=r"https?://(localhost|127\.0\.0\.1)(:\d+)?|moz-extension://.+",
        allow_methods=["GET", "POST"],
        allow_headers=["*"],
        expose_headers=["x-request-id"],
    )

    @app.exception_handler(RequestValidationError)
    async def invalid_request(request, exc):
        # Do not echo request bodies/credentials from pydantic error details.
        first = exc.errors()[0]
        param = ".".join(map(str, first["loc"][1:])) or None
        error = ProxyError(first["msg"], 400, "invalid_request", param)
        return JSONResponse(error.payload(), status_code=400)

    @app.exception_handler(ProxyError)
    async def proxy_error(request, exc):
        headers = {}
        if hasattr(exc, "retry_after"):
            headers["Retry-After"] = str(max(1, int(exc.retry_after)))
        return JSONResponse(exc.payload(), status_code=exc.status, headers=headers)

    @app.post("/v1/chat/completions")
    async def completions(body: ChatRequest, request: Request):
        service = request.app.state.service
        service.validate(body)
        request_id = "req_" + uuid4().hex
        if body.stream:
            return StreamingResponse(
                stream(service, body, request.headers),
                media_type="text/event-stream",
                headers={
                    "x-request-id": request_id,
                    "Cache-Control": "no-cache",
                    "X-Accel-Buffering": "no",
                },
            )
        try:
            return JSONResponse(
                await collect(service, body, request.headers),
                headers={"x-request-id": request_id},
            )
        except Exception as exc:
            raise error_for(exc) from exc

    @app.get("/v1/models")
    async def models():
        return {"object": "list", "data": settings.models}

    @app.get("/healthz")
    async def health():
        return {"status": "ok"}

    @app.get("/readyz")
    async def ready():
        return {"status": "ready", "backend": "lazy_login"}

    return app


app = create_app()


def run():
    import uvicorn

    uvicorn.run(app, host=default_settings.proxy_host, port=default_settings.proxy_port)
