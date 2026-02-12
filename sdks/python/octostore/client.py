"""OctoStore Python client."""

import asyncio
import re
from typing import List, Optional
from urllib.parse import urljoin

import httpx

from .exceptions import (
    AuthenticationError,
    LockError,
    LockHeldError,
    NetworkError,
    ValidationError,
)
from .models import (
    AcquireResult,
    LockInfo,
    LockStatus,
    RenewResult,
    TokenInfo,
    UserLock,
)


class OctoStoreClient:
    """OctoStore distributed lock service client."""
    
    def __init__(self, base_url: str = "https://api.octostore.io", token: str = None):
        """Initialize the OctoStore client.
        
        Args:
            base_url: The base URL of the OctoStore API
            token: Bearer token for authentication
        """
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.client = httpx.Client(
            headers={"Authorization": f"Bearer {token}"} if token else {},
            timeout=30.0,
        )
        self.async_client = None
    
    def _validate_lock_name(self, name: str) -> None:
        """Validate lock name format."""
        if not name or len(name) > 128:
            raise ValidationError("Lock name must be 1-128 characters")
        if not re.match(r'^[a-zA-Z0-9.-]+$', name):
            raise ValidationError("Lock name can only contain alphanumeric characters, hyphens, and dots")
    
    def _validate_ttl(self, ttl: int) -> None:
        """Validate TTL value."""
        if not 1 <= ttl <= 3600:
            raise ValidationError("TTL must be between 1 and 3600 seconds")
    
    def _handle_response(self, response: httpx.Response):
        """Handle HTTP response and raise appropriate exceptions."""
        if response.status_code == 401:
            raise AuthenticationError("Invalid or missing authentication token")
        elif response.status_code >= 400:
            try:
                error_data = response.json()
                message = error_data.get("error", f"HTTP {response.status_code}")
            except:
                message = f"HTTP {response.status_code}: {response.text}"
            raise LockError(message)
        
        try:
            return response.json() if response.content else {}
        except Exception as e:
            raise NetworkError(f"Failed to parse response: {e}")
    
    def health(self) -> str:
        """Check API health status."""
        try:
            response = self.client.get(urljoin(self.base_url, "/health"))
            if response.status_code == 200:
                return response.text.strip('"')
            else:
                raise NetworkError(f"Health check failed: HTTP {response.status_code}")
        except httpx.RequestError as e:
            raise NetworkError(f"Network error during health check: {e}")
    
    def acquire_lock(self, name: str, ttl: int = 60) -> AcquireResult:
        """Acquire a distributed lock.
        
        Args:
            name: Lock name
            ttl: Time-to-live in seconds (1-3600)
            
        Returns:
            AcquireResult with acquisition status and details
        """
        self._validate_lock_name(name)
        self._validate_ttl(ttl)
        
        try:
            response = self.client.post(
                urljoin(self.base_url, f"/locks/{name}/acquire"),
                json={"ttl_seconds": ttl}
            )
            
            if response.status_code == 409:
                data = response.json()
                raise LockHeldError(
                    f"Lock '{name}' is already held",
                    holder_id=data.get("holder_id"),
                    expires_at=data.get("expires_at")
                )
            
            data = self._handle_response(response)
            return AcquireResult(**data)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error acquiring lock: {e}")
    
    def release_lock(self, name: str, lease_id: str) -> None:
        """Release a distributed lock.
        
        Args:
            name: Lock name
            lease_id: Lease ID from acquire operation
        """
        self._validate_lock_name(name)
        
        try:
            response = self.client.post(
                urljoin(self.base_url, f"/locks/{name}/release"),
                json={"lease_id": lease_id}
            )
            self._handle_response(response)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error releasing lock: {e}")
    
    def renew_lock(self, name: str, lease_id: str, ttl: int = 60) -> RenewResult:
        """Renew a distributed lock.
        
        Args:
            name: Lock name
            lease_id: Lease ID from acquire operation
            ttl: New time-to-live in seconds (1-3600)
            
        Returns:
            RenewResult with updated lease information
        """
        self._validate_lock_name(name)
        self._validate_ttl(ttl)
        
        try:
            response = self.client.post(
                urljoin(self.base_url, f"/locks/{name}/renew"),
                json={"lease_id": lease_id, "ttl_seconds": ttl}
            )
            
            data = self._handle_response(response)
            return RenewResult(**data)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error renewing lock: {e}")
    
    def get_lock_status(self, name: str) -> LockInfo:
        """Get the status of a specific lock.
        
        Args:
            name: Lock name
            
        Returns:
            LockInfo with current lock status
        """
        self._validate_lock_name(name)
        
        try:
            response = self.client.get(urljoin(self.base_url, f"/locks/{name}"))
            data = self._handle_response(response)
            
            return LockInfo(
                name=data["name"],
                status=LockStatus(data["status"]),
                holder_id=data.get("holder_id"),
                fencing_token=data["fencing_token"],
                expires_at=data.get("expires_at")
            )
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error getting lock status: {e}")
    
    def list_locks(self) -> List[UserLock]:
        """List all locks owned by the current user.
        
        Returns:
            List of UserLock objects
        """
        try:
            response = self.client.get(urljoin(self.base_url, "/locks"))
            data = self._handle_response(response)
            
            return [UserLock(**lock) for lock in data.get("locks", [])]
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error listing locks: {e}")
    
    def rotate_token(self) -> TokenInfo:
        """Rotate the authentication token.
        
        Returns:
            TokenInfo with new token details
        """
        try:
            response = self.client.post(urljoin(self.base_url, "/auth/token/rotate"))
            data = self._handle_response(response)
            
            # Update the client's token
            new_token = data["token"]
            self.token = new_token
            self.client.headers["Authorization"] = f"Bearer {new_token}"
            
            return TokenInfo(**data)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error rotating token: {e}")
    
    def close(self) -> None:
        """Close the HTTP client."""
        self.client.close()
    
    def __enter__(self):
        """Context manager entry."""
        return self
    
    def __exit__(self, exc_type, exc_val, exc_tb):
        """Context manager exit."""
        self.close()


class AsyncOctoStoreClient:
    """Async OctoStore distributed lock service client."""
    
    def __init__(self, base_url: str = "https://api.octostore.io", token: str = None):
        """Initialize the async OctoStore client.
        
        Args:
            base_url: The base URL of the OctoStore API
            token: Bearer token for authentication
        """
        self.base_url = base_url.rstrip("/")
        self.token = token
        self.client = httpx.AsyncClient(
            headers={"Authorization": f"Bearer {token}"} if token else {},
            timeout=30.0,
        )
    
    def _validate_lock_name(self, name: str) -> None:
        """Validate lock name format."""
        if not name or len(name) > 128:
            raise ValidationError("Lock name must be 1-128 characters")
        if not re.match(r'^[a-zA-Z0-9.-]+$', name):
            raise ValidationError("Lock name can only contain alphanumeric characters, hyphens, and dots")
    
    def _validate_ttl(self, ttl: int) -> None:
        """Validate TTL value."""
        if not 1 <= ttl <= 3600:
            raise ValidationError("TTL must be between 1 and 3600 seconds")
    
    async def _handle_response(self, response: httpx.Response):
        """Handle HTTP response and raise appropriate exceptions."""
        if response.status_code == 401:
            raise AuthenticationError("Invalid or missing authentication token")
        elif response.status_code >= 400:
            try:
                error_data = response.json()
                message = error_data.get("error", f"HTTP {response.status_code}")
            except:
                message = f"HTTP {response.status_code}: {response.text}"
            raise LockError(message)
        
        try:
            return response.json() if response.content else {}
        except Exception as e:
            raise NetworkError(f"Failed to parse response: {e}")
    
    async def health(self) -> str:
        """Check API health status."""
        try:
            response = await self.client.get(urljoin(self.base_url, "/health"))
            if response.status_code == 200:
                return response.text.strip('"')
            else:
                raise NetworkError(f"Health check failed: HTTP {response.status_code}")
        except httpx.RequestError as e:
            raise NetworkError(f"Network error during health check: {e}")
    
    async def acquire_lock(self, name: str, ttl: int = 60) -> AcquireResult:
        """Acquire a distributed lock.
        
        Args:
            name: Lock name
            ttl: Time-to-live in seconds (1-3600)
            
        Returns:
            AcquireResult with acquisition status and details
        """
        self._validate_lock_name(name)
        self._validate_ttl(ttl)
        
        try:
            response = await self.client.post(
                urljoin(self.base_url, f"/locks/{name}/acquire"),
                json={"ttl_seconds": ttl}
            )
            
            if response.status_code == 409:
                data = response.json()
                raise LockHeldError(
                    f"Lock '{name}' is already held",
                    holder_id=data.get("holder_id"),
                    expires_at=data.get("expires_at")
                )
            
            data = await self._handle_response(response)
            return AcquireResult(**data)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error acquiring lock: {e}")
    
    async def release_lock(self, name: str, lease_id: str) -> None:
        """Release a distributed lock.
        
        Args:
            name: Lock name
            lease_id: Lease ID from acquire operation
        """
        self._validate_lock_name(name)
        
        try:
            response = await self.client.post(
                urljoin(self.base_url, f"/locks/{name}/release"),
                json={"lease_id": lease_id}
            )
            await self._handle_response(response)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error releasing lock: {e}")
    
    async def renew_lock(self, name: str, lease_id: str, ttl: int = 60) -> RenewResult:
        """Renew a distributed lock.
        
        Args:
            name: Lock name
            lease_id: Lease ID from acquire operation
            ttl: New time-to-live in seconds (1-3600)
            
        Returns:
            RenewResult with updated lease information
        """
        self._validate_lock_name(name)
        self._validate_ttl(ttl)
        
        try:
            response = await self.client.post(
                urljoin(self.base_url, f"/locks/{name}/renew"),
                json={"lease_id": lease_id, "ttl_seconds": ttl}
            )
            
            data = await self._handle_response(response)
            return RenewResult(**data)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error renewing lock: {e}")
    
    async def get_lock_status(self, name: str) -> LockInfo:
        """Get the status of a specific lock.
        
        Args:
            name: Lock name
            
        Returns:
            LockInfo with current lock status
        """
        self._validate_lock_name(name)
        
        try:
            response = await self.client.get(urljoin(self.base_url, f"/locks/{name}"))
            data = await self._handle_response(response)
            
            return LockInfo(
                name=data["name"],
                status=LockStatus(data["status"]),
                holder_id=data.get("holder_id"),
                fencing_token=data["fencing_token"],
                expires_at=data.get("expires_at")
            )
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error getting lock status: {e}")
    
    async def list_locks(self) -> List[UserLock]:
        """List all locks owned by the current user.
        
        Returns:
            List of UserLock objects
        """
        try:
            response = await self.client.get(urljoin(self.base_url, "/locks"))
            data = await self._handle_response(response)
            
            return [UserLock(**lock) for lock in data.get("locks", [])]
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error listing locks: {e}")
    
    async def rotate_token(self) -> TokenInfo:
        """Rotate the authentication token.
        
        Returns:
            TokenInfo with new token details
        """
        try:
            response = await self.client.post(urljoin(self.base_url, "/auth/token/rotate"))
            data = await self._handle_response(response)
            
            # Update the client's token
            new_token = data["token"]
            self.token = new_token
            self.client.headers["Authorization"] = f"Bearer {new_token}"
            
            return TokenInfo(**data)
            
        except httpx.RequestError as e:
            raise NetworkError(f"Network error rotating token: {e}")
    
    async def close(self) -> None:
        """Close the HTTP client."""
        await self.client.aclose()
    
    async def __aenter__(self):
        """Async context manager entry."""
        return self
    
    async def __aexit__(self, exc_type, exc_val, exc_tb):
        """Async context manager exit."""
        await self.close()