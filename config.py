"""Compatibility alias; implementation lives in deepseek_proxy.settings."""

import sys
from importlib import import_module

sys.modules[__name__] = import_module("deepseek_proxy.settings")
