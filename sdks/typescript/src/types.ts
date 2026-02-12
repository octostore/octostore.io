/**
 * OctoStore TypeScript SDK types and interfaces
 */

export enum LockStatus {
  FREE = 'free',
  HELD = 'held',
}

export interface LockInfo {
  name: string;
  status: LockStatus;
  holder_id: string | null;
  fencing_token: number;
  expires_at: string | null;
}

export interface AcquireResult {
  status: 'acquired' | 'held';
  lease_id?: string;
  fencing_token?: number;
  expires_at?: string;
  holder_id?: string;
}

export interface RenewResult {
  lease_id: string;
  expires_at: string;
}

export interface TokenInfo {
  token: string;
  user_id: string;
  github_username: string;
}

export interface UserLock {
  name: string;
  lease_id: string;
  fencing_token: number;
  expires_at: string;
}

export interface AcquireRequest {
  ttl_seconds: number;
}

export interface ReleaseRequest {
  lease_id: string;
}

export interface RenewRequest {
  lease_id: string;
  ttl_seconds: number;
}

export interface ListLocksResponse {
  locks: UserLock[];
}

export interface OctoStoreClientOptions {
  baseUrl?: string;
  token?: string;
  timeout?: number;
}

export class OctoStoreError extends Error {
  constructor(message: string, public statusCode?: number) {
    super(message);
    this.name = 'OctoStoreError';
  }
}

export class AuthenticationError extends OctoStoreError {
  constructor(message: string = 'Authentication failed') {
    super(message, 401);
    this.name = 'AuthenticationError';
  }
}

export class NetworkError extends OctoStoreError {
  constructor(message: string, public originalError?: unknown) {
    super(message);
    this.name = 'NetworkError';
  }
}

export class LockError extends OctoStoreError {
  constructor(message: string, statusCode?: number) {
    super(message, statusCode);
    this.name = 'LockError';
  }
}

export class LockHeldError extends LockError {
  constructor(
    message: string,
    public holder_id?: string,
    public expires_at?: string
  ) {
    super(message, 409);
    this.name = 'LockHeldError';
  }
}

export class ValidationError extends OctoStoreError {
  constructor(message: string) {
    super(message, 400);
    this.name = 'ValidationError';
  }
}