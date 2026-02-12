# OctoStore TypeScript/JavaScript SDK

TypeScript/JavaScript client library for the OctoStore distributed lock service. Works in Node.js and browsers.

## Installation

```bash
npm install @octostore/client
```

Or with yarn:

```bash
yarn add @octostore/client
```

## Quick Start

### Node.js / TypeScript

```typescript
import { OctoStoreClient, LockHeldError } from '@octostore/client';

// Initialize client
const client = new OctoStoreClient({
  baseUrl: 'https://api.octostore.io',
  token: 'your-token-here'
});

async function main() {
  // Check service health
  const health = await client.health();
  console.log(`Service status: ${health}`);

  try {
    // Acquire a lock
    const result = await client.acquireLock('my-resource', 300);
    console.log(`Lock acquired: ${result.lease_id}`);
    
    // Do your critical work here...
    
    // Release the lock
    await client.releaseLock('my-resource', result.lease_id!);
    console.log('Lock released');
    
  } catch (error) {
    if (error instanceof LockHeldError) {
      console.log(`Lock is held by ${error.holder_id} until ${error.expires_at}`);
    } else {
      console.error('Error:', error.message);
    }
  }
}

main().catch(console.error);
```

### Browser / JavaScript

```html
<script type="module">
import { OctoStoreClient } from 'https://unpkg.com/@octostore/client@1.0.0/dist/index.mjs';

const client = new OctoStoreClient({
  token: 'your-token-here'
});

try {
  const health = await client.health();
  console.log('Service healthy:', health);
  
  const result = await client.acquireLock('browser-lock', 120);
  console.log('Lock acquired:', result);
} catch (error) {
  console.error('Error:', error.message);
}
</script>
```

### CommonJS

```javascript
const { OctoStoreClient, LockHeldError } = require('@octostore/client');

const client = new OctoStoreClient({
  baseUrl: process.env.OCTOSTORE_BASE_URL || 'https://api.octostore.io',
  token: process.env.OCTOSTORE_TOKEN
});

async function acquireLock() {
  try {
    const result = await client.acquireLock('my-lock', 60);
    
    // Critical section
    console.log('Working with lock:', result.lease_id);
    
    // Auto-renew if needed
    const renewed = await client.renewLock('my-lock', result.lease_id, 120);
    console.log('Lock renewed until:', renewed.expires_at);
    
    // Release when done
    await client.releaseLock('my-lock', result.lease_id);
    
  } catch (error) {
    if (error instanceof LockHeldError) {
      console.log('Lock currently held, waiting...');
    } else {
      throw error;
    }
  }
}
```

## API Reference

### OctoStoreClient

#### Constructor

```typescript
new OctoStoreClient(options?: OctoStoreClientOptions)
```

Options:
- `baseUrl?: string` - API base URL (default: "https://api.octostore.io")
- `token?: string` - Bearer token for authentication
- `timeout?: number` - Request timeout in milliseconds (default: 30000)

#### Methods

##### `health(): Promise<string>`
Check service health status.

##### `acquireLock(name: string, ttl?: number): Promise<AcquireResult>`
Acquire a distributed lock.
- `name` - Lock name (alphanumeric, hyphens, dots only, max 128 chars)
- `ttl` - Time-to-live in seconds (1-3600, default: 60)

##### `releaseLock(name: string, leaseId: string): Promise<void>`
Release a distributed lock.

##### `renewLock(name: string, leaseId: string, ttl?: number): Promise<RenewResult>`
Renew a distributed lock.

##### `getLockStatus(name: string): Promise<LockInfo>`
Get the current status of a lock.

##### `listLocks(): Promise<UserLock[]>`
List all locks owned by the current user.

##### `rotateToken(): Promise<TokenInfo>`
Rotate the authentication token.

### Types

```typescript
interface AcquireResult {
  status: 'acquired' | 'held';
  lease_id?: string;
  fencing_token?: number;
  expires_at?: string;
  holder_id?: string;
}

interface LockInfo {
  name: string;
  status: 'free' | 'held';
  holder_id: string | null;
  fencing_token: number;
  expires_at: string | null;
}

interface UserLock {
  name: string;
  lease_id: string;
  fencing_token: number;
  expires_at: string;
}
```

### Error Handling

```typescript
import {
  OctoStoreError,        // Base error class
  AuthenticationError,   // 401 - Invalid token
  NetworkError,         // Network/timeout issues
  LockError,            // General lock errors
  LockHeldError,        // 409 - Lock currently held
  ValidationError,      // Invalid parameters
} from '@octostore/client';

try {
  await client.acquireLock('resource', 60);
} catch (error) {
  if (error instanceof LockHeldError) {
    console.log(`Lock held by: ${error.holder_id}`);
  } else if (error instanceof ValidationError) {
    console.log(`Invalid input: ${error.message}`);
  } else if (error instanceof NetworkError) {
    console.log(`Network issue: ${error.message}`);
  }
}
```

## Environment Variables

```bash
export OCTOSTORE_BASE_URL="https://api.octostore.io"
export OCTOSTORE_TOKEN="your-token-here"
```

## Browser Support

This SDK supports all modern browsers with:
- ES2020+ features
- fetch() API (or polyfill)
- AbortController (or polyfill)

For older browsers, use a bundler like Webpack or Rollup with appropriate polyfills.

## Best Practices

1. **Set appropriate timeouts** for your use case
2. **Handle network errors** with retry logic
3. **Use TypeScript** for better development experience
4. **Validate lock names** on the client side
5. **Implement exponential backoff** for lock acquisition retries

## License

MIT License