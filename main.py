"""Compatibility entry point: uvicorn main:app or python main.py."""

from deepseek_proxy.app import app, create_app, run

__all__ = ["app", "create_app", "run"]

if __name__ == "__main__":
    run()
