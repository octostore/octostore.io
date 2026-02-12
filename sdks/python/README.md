# OctoStore Python SDK

Python client library for the OctoStore distributed lock service.

## Installation

```bash
pip install octostore
```

## Quick Start

### Synchronous Client

```python
from octostore import OctoStoreClient, LockHeldError

# Initialize client
client = OctoStoreClient(
    base_url="https://api.octostore.io",
    token="your-token-here"
)

# Check service health
health = client.health()
print(f"Service status: {health}")

# Acquire a lock
try:
    result = client.acquire_lock("my-resource", ttl=300)
    print(f"Lock acquired: {result.lease_id}")
    
    # Do your critical work here...
    
    # Release the lock
    client.release_lock("my-resource", result.lease_id)
    print("Lock released")
    
except LockHeldError as e:
    print(f"Lock is held by {e.holder_id} until {e.expires_at}")

# Context manager (automatically releases)
with client:
    try:
        result = client.acquire_lock("my-resource", ttl=60)
        # Work with the lock...
    except LockHeldError:
        print("Lock is currently held")
```

### Asynchronous Client

```python
import asyncio
from octostore import AsyncOctoStoreClient

async def main():
    async with AsyncOctoStoreClient(token="your-token") as client:
        # Health check
        health = await client.health()
        print(f"Service status: {health}")
        
        # Acquire lock
        result = await client.acquire_lock("async-resource", ttl=120)
        print(f"Lock acquired: {result.lease_id}")
        
        # Renew lock
        renewed = await client.renew_lock("async-resource", result.lease_id, ttl=180)
        print(f"Lock renewed until: {renewed.expires_at}")
        
        # Release lock
        await client.release_lock("async-resource", result.lease_id)

asyncio.run(main())
```

## API Reference

### OctoStoreClient

#### Constructor

```python
client = OctoStoreClient(base_url="https://api.octostore.io", token="your-token")
```

#### Methods

- `health() -> str` - Check service health
- `acquire_lock(name: str, ttl: int = 60) -> AcquireResult` - Acquire a lock
- `release_lock(name: str, lease_id: str) -> None` - Release a lock
- `renew_lock(name: str, lease_id: str, ttl: int = 60) -> RenewResult` - Renew a lock
- `get_lock_status(name: str) -> LockInfo` - Get lock status
- `list_locks() -> List[UserLock]` - List user's locks
- `rotate_token() -> TokenInfo` - Rotate authentication token

### Data Models

#### AcquireResult
- `status: str` - "acquired" or "held" 
- `lease_id: Optional[str]` - Lease ID for acquired locks
- `fencing_token: Optional[int]` - Fencing token for coordination
- `expires_at: Optional[str]` - Lock expiration time
- `holder_id: Optional[str]` - Current holder (if held)

#### LockInfo
- `name: str` - Lock name
- `status: LockStatus` - Current status (FREE or HELD)
- `holder_id: Optional[str]` - Current holder
- `fencing_token: int` - Current fencing token
- `expires_at: Optional[str]` - Expiration time

### Exception Handling

```python
from octostore import (
    OctoStoreError,          # Base exception
    AuthenticationError,     # Invalid token
    NetworkError,           # Connection issues
    LockError,              # Lock operation errors
    LockHeldError,          # Lock currently held
    ValidationError,        # Invalid parameters
)

try:
    result = client.acquire_lock("resource", ttl=60)
except LockHeldError as e:
    print(f"Lock held by {e.holder_id}")
except ValidationError as e:
    print(f"Invalid input: {e}")
except NetworkError as e:
    print(f"Network issue: {e}")
```

## Configuration

### Environment Variables

```bash
export OCTOSTORE_BASE_URL="https://api.octostore.io"
export OCTOSTORE_TOKEN="your-token-here"
```

```python
import os
from octostore import OctoStoreClient

client = OctoStoreClient(
    base_url=os.getenv("OCTOSTORE_BASE_URL", "https://api.octostore.io"),
    token=os.getenv("OCTOSTORE_TOKEN")
)
```

## Best Practices

1. **Use context managers** to ensure proper cleanup
2. **Handle LockHeldError** gracefully in concurrent scenarios
3. **Set appropriate TTL** values for your use case
4. **Implement retry logic** for network errors
5. **Use fencing tokens** for coordination in distributed systems

## License

MIT License