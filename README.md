# Octostore Lock

A distributed locking service with leader election capabilities. Simple, fast, and reliable.

## Features

- **GitHub OAuth Authentication** - Secure user management via GitHub
- **Distributed Locks** - Acquire, release, and renew locks with TTL
- **Fencing Tokens** - Monotonically increasing tokens for safe distributed operations
- **Lock Limits** - Maximum 100 locks per user account
- **Background Expiry** - Automatic cleanup of expired locks
- **RESTful API** - Simple HTTP interface
- **Minimal Dependencies** - Single binary, no external services required

## Quick Start

### 1. Set up GitHub OAuth App

1. Go to GitHub → Settings → Developer settings → OAuth Apps
2. Create a new OAuth app with:
   - Application name: `Octostore Lock`
   - Homepage URL: `http://localhost:3000`
   - Authorization callback URL: `http://localhost:3000/auth/github/callback`
3. Note down the Client ID and Client Secret

### 2. Configure Environment

Copy the example environment file:
```bash
cp .env.example .env
```

Edit `.env` with your GitHub OAuth credentials:
```bash
GITHUB_CLIENT_ID=your_github_client_id
GITHUB_CLIENT_SECRET=your_github_client_secret
GITHUB_REDIRECT_URI=http://localhost:3000/auth/github/callback
DATABASE_URL=octostore.db
BIND_ADDR=0.0.0.0:3000
```

### 3. Run the Service

```bash
cargo run
```

The service will start on `http://localhost:3000`

## API Usage

### Authentication

All lock endpoints require a Bearer token in the `Authorization` header.

#### Get GitHub OAuth URL
```bash
curl http://localhost:3000/auth/github
# Follow the redirect to authorize with GitHub
```

#### Handle OAuth Callback
The callback endpoint returns your bearer token:
```json
{
  "token": "base64-encoded-token",
  "user_id": "uuid",
  "github_username": "your-username"
}
```

#### Rotate Token
```bash
curl -X POST http://localhost:3000/auth/token/rotate \
  -H "Authorization: Bearer YOUR_TOKEN"
```

### Lock Operations

#### Acquire Lock
```bash
curl -X POST http://localhost:3000/locks/my-service/acquire \
  -H "Authorization: Bearer YOUR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"ttl_seconds": 300}'
```

Response (acquired):
```json
{
  "status": "acquired",
  "lease_id": "uuid",
  "fencing_token": 42,
  "expires_at": "2024-01-01T12:00:00Z"
}
```

Response (held by another user):
```json
{
  "status": "held",
  "holder_id": "other-user-uuid",
  "expires_at": "2024-01-01T12:05:00Z"
}
```

#### Release Lock
```bash
curl -X POST http://localhost:3000/locks/my-service/release \
  -H "Authorization: Bearer YOUR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"lease_id": "your-lease-uuid"}'
```

#### Renew Lock
```bash
curl -X POST http://localhost:3000/locks/my-service/renew \
  -H "Authorization: Bearer YOUR_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"lease_id": "your-lease-uuid", "ttl_seconds": 300}'
```

#### Get Lock Status
```bash
curl http://localhost:3000/locks/my-service \
  -H "Authorization: Bearer YOUR_TOKEN"
```

Response:
```json
{
  "name": "my-service",
  "status": "held",
  "holder_id": "user-uuid",
  "fencing_token": 42,
  "expires_at": "2024-01-01T12:00:00Z"
}
```

#### List Your Locks
```bash
curl http://localhost:3000/locks \
  -H "Authorization: Bearer YOUR_TOKEN"
```

Response:
```json
{
  "locks": [
    {
      "name": "my-service",
      "lease_id": "uuid",
      "fencing_token": 42,
      "expires_at": "2024-01-01T12:00:00Z"
    }
  ]
}
```

## Lock Constraints

- **Lock Names**: Alphanumeric characters, hyphens, and dots only. Max 128 characters.
- **TTL Range**: 1 to 3600 seconds (1 hour maximum)
- **User Limit**: Maximum 100 active locks per user account
- **Fencing Tokens**: Globally unique, monotonically increasing integers

## Using Fencing Tokens

Fencing tokens help prevent split-brain scenarios in distributed systems:

```python
# Example: Safe database operation with fencing token
def write_to_database(data, fencing_token):
    # Include fencing token in your database operation
    query = "UPDATE table SET data = ?, fencing_token = ? WHERE fencing_token < ?"
    result = db.execute(query, (data, fencing_token, fencing_token))
    
    if result.rowcount == 0:
        raise Exception("Operation rejected - newer fencing token exists")
```

## Architecture

- **Axum** - Web framework
- **Tokio** - Async runtime
- **DashMap** - Concurrent in-memory lock storage
- **SQLite** - Persistent user accounts and fencing token counter
- **reqwest** - GitHub OAuth client

## Development

### Build
```bash
cargo build --release
```

### Test
```bash
cargo test
```

### Check (fast compile check)
```bash
cargo check
```

## License

MIT