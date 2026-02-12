"""OctoStore Python SDK - Distributed lock service client."""

from .client import OctoStoreClient
from .exceptions import (
    OctoStoreError,
    AuthenticationError,
    NetworkError,
    LockError,
    LockHeldError,
    ValidationError,
)
from .models import (
    LockStatus,
    LockInfo,
    AcquireResult,
    RenewResult,
    TokenInfo,
)

__version__ = "1.0.0"
__all__ = [
    "OctoStoreClient",
    "OctoStoreError",
    "AuthenticationError", 
    "NetworkError",
    "LockError",
    "LockHeldError",
    "ValidationError",
    "LockStatus",
    "LockInfo", 
    "AcquireResult",
    "RenewResult",
    "TokenInfo",
]