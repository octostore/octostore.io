# OctoStore Rust SDK

Async Rust client library for the OctoStore distributed lock service.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
octostore = "1.0"
tokio = { version = "1.0", features = ["full"] }
```

## Quick Start

```rust
use octostore::{Client, Error, LockHeldError};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize client
    let client = Client::new("https://api.octostore.io", "your-token-here")?;

    // Check service health
    let health = client.health().await?;
    println!("Service status: {}", health);

    // Acquire a lock
    match client.acquire_lock("my-resource", 300).await {
        Ok(result) => {
            println!("Lock acquired: {}", result.lease_id.unwrap());
            
            // Do your critical work here...
            
            // Release the lock
            client.release_lock("my-resource", &result.lease_id.unwrap()).await?;
            println!("Lock released");
        }
        Err(Error::LockHeld(lock_err)) => {
            println!("Lock is held by {} until {}", 
                lock_err.holder_id.unwrap_or_default(), 
                lock_err.expires_at.unwrap_or_default());
        }
        Err(e) => return Err(e.into()),
    }

    Ok(())
}
```

## Advanced Usage

### Lock with Timeout

```rust
use std::time::Duration;
use tokio::time::timeout;

async fn acquire_with_timeout() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("https://api.octostore.io", "your-token")?;
    
    // Try to acquire lock with 5-second timeout
    match timeout(Duration::from_secs(5), client.acquire_lock("resource", 60)).await {
        Ok(Ok(result)) => {
            println!("Lock acquired: {:?}", result);
            // Use the lock...
        }
        Ok(Err(Error::LockHeld(_))) => {
            println!("Lock is held by another process");
        }
        Err(_) => {
            println!("Timed out waiting for lock");
        }
        Ok(Err(e)) => {
            return Err(e.into());
        }
    }
    
    Ok(())
}
```

### Automatic Lock Renewal

```rust
use std::time::Duration;
use tokio::time::{interval, sleep};

async fn long_running_task_with_renewal() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("https://api.octostore.io", "your-token")?;
    
    // Acquire lock
    let result = client.acquire_lock("long-task", 60).await?;
    let lease_id = result.lease_id.unwrap();
    
    // Start renewal task
    let renewal_client = client.clone();
    let renewal_lease_id = lease_id.clone();
    let renewal_task = tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(30));
        
        loop {
            interval.tick().await;
            match renewal_client.renew_lock("long-task", &renewal_lease_id, 60).await {
                Ok(renewed) => {
                    println!("Lock renewed until: {}", renewed.expires_at);
                }
                Err(e) => {
                    eprintln!("Failed to renew lock: {}", e);
                    break;
                }
            }
        }
    });
    
    // Do long-running work
    sleep(Duration::from_secs(120)).await;
    
    // Cancel renewal and release lock
    renewal_task.abort();
    client.release_lock("long-task", &lease_id).await?;
    
    Ok(())
}
```

### Lock Status Monitoring

```rust
use octostore::{Client, LockStatus};

async fn monitor_lock_status() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("https://api.octostore.io", "your-token")?;
    
    let status = client.get_lock_status("monitored-resource").await?;
    
    println!("Lock '{}' is {:?}", status.name, status.status);
    
    match status.status {
        LockStatus::Held => {
            if let Some(holder_id) = &status.holder_id {
                println!("Held by: {}", holder_id);
            }
            if let Some(expires_at) = &status.expires_at {
                println!("Expires: {}", expires_at);
            }
            println!("Fencing token: {}", status.fencing_token);
        }
        LockStatus::Free => {
            println!("Lock is available for acquisition");
        }
    }
    
    Ok(())
}
```

### List User Locks

```rust
async fn list_my_locks() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new("https://api.octostore.io", "your-token")?;
    
    let locks = client.list_locks().await?;
    
    println!("You have {} active locks:", locks.len());
    for lock in locks {
        println!("- {} (expires: {})", lock.name, lock.expires_at);
    }
    
    Ok(())
}
```

## API Reference

### Client

#### `Client::new(base_url: &str, token: &str) -> Result<Client>`
Create a new authenticated client.

#### `Client::new_without_auth(base_url: &str) -> Result<Client>`
Create a new client without authentication (for public endpoints).

#### `Client::with_timeout(self, timeout: Duration) -> Result<Client>`
Set custom timeout for HTTP requests.

### Methods

#### `health(&self) -> Result<String>`
Check API health status.

#### `acquire_lock(&self, name: &str, ttl: u32) -> Result<AcquireResult>`
Acquire a distributed lock.
- `name` - Lock name (alphanumeric, hyphens, dots only, max 128 chars)
- `ttl` - Time-to-live in seconds (1-3600)

#### `release_lock(&self, name: &str, lease_id: &str) -> Result<()>`
Release a distributed lock.

#### `renew_lock(&self, name: &str, lease_id: &str, ttl: u32) -> Result<RenewResult>`
Renew a distributed lock.

#### `get_lock_status(&self, name: &str) -> Result<LockInfo>`
Get the current status of a lock.

#### `list_locks(&self) -> Result<Vec<UserLock>>`
List all locks owned by the current user.

#### `rotate_token(&mut self) -> Result<TokenInfo>`
Rotate the authentication token.

### Types

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AcquireResult {
    pub status: String,
    pub lease_id: Option<String>,
    pub fencing_token: Option<u64>,
    pub expires_at: Option<String>,
    pub holder_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LockInfo {
    pub name: String,
    pub status: LockStatus,
    pub holder_id: Option<String>,
    pub fencing_token: u64,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStatus {
    Free,
    Held,
}
```

### Error Handling

```rust
use octostore::{Error, LockHeldError};

match client.acquire_lock("resource", 60).await {
    Ok(result) => {
        // Lock acquired successfully
        println!("Acquired: {:?}", result);
    }
    Err(Error::LockHeld(lock_err)) => {
        println!("Lock held by: {:?}", lock_err.holder_id);
        println!("Expires at: {:?}", lock_err.expires_at);
    }
    Err(Error::Authentication { message }) => {
        eprintln!("Auth failed: {}", message);
    }
    Err(Error::Network { source }) => {
        eprintln!("Network error: {}", source);
    }
    Err(Error::Validation { message }) => {
        eprintln!("Invalid input: {}", message);
    }
    Err(e) => {
        eprintln!("Other error: {}", e);
    }
}
```

## Features

### TLS Options

```toml
[dependencies]
octostore = { version = "1.0", features = ["rustls"] }
# or
octostore = { version = "1.0", features = ["native-tls"] }
```

## Environment Variables

```rust
use std::env;

fn create_client() -> Result<Client, Box<dyn std::error::Error>> {
    let base_url = env::var("OCTOSTORE_BASE_URL")
        .unwrap_or_else(|_| "https://api.octostore.io".to_string());
    let token = env::var("OCTOSTORE_TOKEN")
        .expect("OCTOSTORE_TOKEN environment variable required");
    
    Ok(Client::new(&base_url, &token)?)
}
```

## Best Practices

1. **Use structured error handling** with match statements
2. **Implement retry logic** with exponential backoff
3. **Set appropriate timeouts** for your use case  
4. **Use fencing tokens** for coordination in distributed systems
5. **Clone the client** for use in multiple async tasks
6. **Handle lock renewal** for long-running operations

## License

MIT License