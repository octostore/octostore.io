"""OctoStore data models."""

from dataclasses import dataclass
from enum import Enum
from typing import Optional, List


class LockStatus(str, Enum):
    """Lock status enumeration."""
    FREE = "free"
    HELD = "held"


@dataclass
class LockInfo:
    """Information about a lock."""
    name: str
    status: LockStatus
    holder_id: Optional[str]
    fencing_token: int
    expires_at: Optional[str]


@dataclass 
class AcquireResult:
    """Result of lock acquisition attempt."""
    status: str
    lease_id: Optional[str] = None
    fencing_token: Optional[int] = None
    expires_at: Optional[str] = None
    holder_id: Optional[str] = None


@dataclass
class RenewResult:
    """Result of lock renewal."""
    lease_id: str
    expires_at: str


@dataclass
class TokenInfo:
    """Token rotation result."""
    token: str
    user_id: str
    github_username: str


@dataclass
class UserLock:
    """A lock owned by the user."""
    name: str
    lease_id: str
    fencing_token: int
    expires_at: str