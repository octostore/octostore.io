"""OctoStore exception classes."""

class OctoStoreError(Exception):
    """Base exception for all OctoStore errors."""
    pass


class AuthenticationError(OctoStoreError):
    """Raised when authentication fails."""
    pass


class NetworkError(OctoStoreError):
    """Raised when network operations fail."""
    pass


class LockError(OctoStoreError):
    """Raised when lock operations fail."""
    pass


class LockHeldError(LockError):
    """Raised when trying to acquire a lock that's already held."""
    
    def __init__(self, message: str, holder_id: str = None, expires_at: str = None):
        super().__init__(message)
        self.holder_id = holder_id
        self.expires_at = expires_at


class ValidationError(OctoStoreError):
    """Raised when input validation fails."""
    pass