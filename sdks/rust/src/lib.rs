//! # OctoStore Rust SDK
//!
//! Rust client library for the OctoStore distributed lock service.
//!
//! ## Quick Start
//!
//! ```no_run
//! use octostore::{Client, Error, LockHeldError};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Initialize client
//!     let client = Client::new("https://api.octostore.io", "your-token-here")?;
//!
//!     // Check service health
//!     let health = client.health().await?;
//!     println!("Service status: {}", health);
//!
//!     // Acquire a lock
//!     match client.acquire_lock("my-resource", 300).await {
//!         Ok(result) => {
//!             println!("Lock acquired: {}", result.lease_id.unwrap());
//!             
//!             // Do your critical work here...
//!             
//!             // Release the lock
//!             client.release_lock("my-resource", &result.lease_id.unwrap()).await?;
//!             println!("Lock released");
//!         }
//!         Err(Error::LockHeld(lock_err)) => {
//!             println!("Lock is held by {} until {}", 
//!                 lock_err.holder_id.unwrap_or_default(), 
//!                 lock_err.expires_at.unwrap_or_default());
//!         }
//!         Err(e) => return Err(e.into()),
//!     }
//!
//!     Ok(())
//! }
//! ```

pub mod client;
pub mod error;
pub mod types;

pub use client::Client;
pub use error::{Error, LockHeldError, Result};
pub use types::*;