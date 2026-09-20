"""Public API errors; backend details belong in server logs."""


class ProxyError(Exception):
    def __init__(self, message, status=502, code="backend_error", param=None):
        super().__init__(message)
        self.status = status
        self.code = code
        self.param = param

    def payload(self):
        return {
            "error": {
                "message": str(self),
                "type": "invalid_request_error" if self.status == 400 else "server_error",
                "param": self.param,
                "code": self.code,
            }
        }
