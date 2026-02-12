/**
 * OctoStore TypeScript/JavaScript client
 */

import {
  AcquireRequest,
  AcquireResult,
  AuthenticationError,
  LockError,
  LockHeldError,
  LockInfo,
  LockStatus,
  ListLocksResponse,
  NetworkError,
  OctoStoreClientOptions,
  OctoStoreError,
  ReleaseRequest,
  RenewRequest,
  RenewResult,
  TokenInfo,
  UserLock,
  ValidationError,
} from './types';

/**
 * OctoStore distributed lock service client
 */
export class OctoStoreClient {
  private readonly baseUrl: string;
  private token: string | null;
  private readonly timeout: number;

  constructor(options: OctoStoreClientOptions = {}) {
    this.baseUrl = options.baseUrl?.replace(/\/$/, '') || 'https://api.octostore.io';
    this.token = options.token || null;
    this.timeout = options.timeout || 30000;
  }

  /**
   * Validate lock name format
   */
  private validateLockName(name: string): void {
    if (!name || name.length > 128) {
      throw new ValidationError('Lock name must be 1-128 characters');
    }
    if (!/^[a-zA-Z0-9.-]+$/.test(name)) {
      throw new ValidationError('Lock name can only contain alphanumeric characters, hyphens, and dots');
    }
  }

  /**
   * Validate TTL value
   */
  private validateTtl(ttl: number): void {
    if (!Number.isInteger(ttl) || ttl < 1 || ttl > 3600) {
      throw new ValidationError('TTL must be an integer between 1 and 3600 seconds');
    }
  }

  /**
   * Make HTTP request with proper error handling
   */
  private async request<T = any>(
    path: string,
    options: RequestInit = {}
  ): Promise<T> {
    const url = `${this.baseUrl}${path}`;
    const headers: HeadersInit = {
      'Content-Type': 'application/json',
      ...options.headers,
    };

    if (this.token) {
      headers['Authorization'] = `Bearer ${this.token}`;
    }

    const requestOptions: RequestInit = {
      ...options,
      headers,
      signal: AbortSignal.timeout(this.timeout),
    };

    try {
      const response = await fetch(url, requestOptions);
      
      if (response.status === 401) {
        throw new AuthenticationError('Invalid or missing authentication token');
      }

      if (response.status === 409) {
        const errorData = await response.json().catch(() => ({}));
        if (path.includes('/acquire')) {
          throw new LockHeldError(
            `Lock is already held`,
            errorData.holder_id,
            errorData.expires_at
          );
        }
      }

      if (!response.ok) {
        let errorMessage: string;
        try {
          const errorData = await response.json();
          errorMessage = errorData.error || `HTTP ${response.status}`;
        } catch {
          errorMessage = `HTTP ${response.status}: ${response.statusText}`;
        }
        throw new LockError(errorMessage, response.status);
      }

      // Handle empty responses
      const text = await response.text();
      if (!text) {
        return {} as T;
      }

      // Handle plain text responses (like health check)
      if (response.headers.get('content-type')?.includes('application/json')) {
        return JSON.parse(text);
      } else {
        return text as T;
      }
    } catch (error) {
      if (error instanceof OctoStoreError) {
        throw error;
      }
      
      if (error instanceof DOMException && error.name === 'TimeoutError') {
        throw new NetworkError('Request timeout');
      }
      
      throw new NetworkError(`Network error: ${error instanceof Error ? error.message : 'Unknown error'}`, error);
    }
  }

  /**
   * Check API health status
   */
  async health(): Promise<string> {
    return this.request<string>('/health');
  }

  /**
   * Acquire a distributed lock
   */
  async acquireLock(name: string, ttl: number = 60): Promise<AcquireResult> {
    this.validateLockName(name);
    this.validateTtl(ttl);

    const body: AcquireRequest = { ttl_seconds: ttl };
    return this.request<AcquireResult>(`/locks/${encodeURIComponent(name)}/acquire`, {
      method: 'POST',
      body: JSON.stringify(body),
    });
  }

  /**
   * Release a distributed lock
   */
  async releaseLock(name: string, leaseId: string): Promise<void> {
    this.validateLockName(name);

    const body: ReleaseRequest = { lease_id: leaseId };
    await this.request(`/locks/${encodeURIComponent(name)}/release`, {
      method: 'POST',
      body: JSON.stringify(body),
    });
  }

  /**
   * Renew a distributed lock
   */
  async renewLock(name: string, leaseId: string, ttl: number = 60): Promise<RenewResult> {
    this.validateLockName(name);
    this.validateTtl(ttl);

    const body: RenewRequest = { lease_id: leaseId, ttl_seconds: ttl };
    return this.request<RenewResult>(`/locks/${encodeURIComponent(name)}/renew`, {
      method: 'POST',
      body: JSON.stringify(body),
    });
  }

  /**
   * Get the status of a specific lock
   */
  async getLockStatus(name: string): Promise<LockInfo> {
    this.validateLockName(name);

    const data = await this.request<any>(`/locks/${encodeURIComponent(name)}`);
    return {
      name: data.name,
      status: data.status as LockStatus,
      holder_id: data.holder_id || null,
      fencing_token: data.fencing_token,
      expires_at: data.expires_at || null,
    };
  }

  /**
   * List all locks owned by the current user
   */
  async listLocks(): Promise<UserLock[]> {
    const data = await this.request<ListLocksResponse>('/locks');
    return data.locks || [];
  }

  /**
   * Rotate the authentication token
   */
  async rotateToken(): Promise<TokenInfo> {
    const tokenInfo = await this.request<TokenInfo>('/auth/token/rotate', {
      method: 'POST',
    });
    
    // Update the client's token
    this.token = tokenInfo.token;
    return tokenInfo;
  }

  /**
   * Update the authentication token
   */
  setToken(token: string): void {
    this.token = token;
  }

  /**
   * Get the current authentication token
   */
  getToken(): string | null {
    return this.token;
  }
}