require 'faraday'
require 'json'

module OctoStore
  class Client
    attr_reader :base_url, :token

    def initialize(base_url: 'https://api.octostore.io', token: nil, timeout: 30)
      @base_url = base_url.chomp('/')
      @token = token
      @connection = Faraday.new(@base_url) do |conn|
        conn.request :json
        conn.response :json
        conn.adapter Faraday.default_adapter
        conn.options.timeout = timeout
        conn.headers['Authorization'] = "Bearer #{@token}" if @token
      end
    end

    def health
      response = @connection.get('/health')
      handle_response(response)
    end

    def acquire_lock(name, ttl: 60)
      validate_lock_name(name)
      validate_ttl(ttl)

      response = @connection.post("/locks/#{name}/acquire", { ttl_seconds: ttl })
      
      if response.status == 409
        data = response.body
        raise LockHeldError.new("Lock '#{name}' is already held", data['holder_id'], data['expires_at'])
      end
      
      handle_response(response)
    end

    def release_lock(name, lease_id)
      validate_lock_name(name)
      response = @connection.post("/locks/#{name}/release", { lease_id: lease_id })
      handle_response(response)
    end

    def renew_lock(name, lease_id, ttl: 60)
      validate_lock_name(name)
      validate_ttl(ttl)
      
      response = @connection.post("/locks/#{name}/renew", { lease_id: lease_id, ttl_seconds: ttl })
      handle_response(response)
    end

    def get_lock_status(name)
      validate_lock_name(name)
      response = @connection.get("/locks/#{name}")
      handle_response(response)
    end

    def list_locks
      response = @connection.get('/locks')
      result = handle_response(response)
      result['locks'] || []
    end

    def rotate_token
      response = @connection.post('/auth/token/rotate')
      token_info = handle_response(response)
      @token = token_info['token']
      @connection.headers['Authorization'] = "Bearer #{@token}"
      token_info
    end

    private

    def validate_lock_name(name)
      raise ValidationError, "Lock name must be 1-128 characters" if name.nil? || name.empty? || name.length > 128
      raise ValidationError, "Lock name can only contain alphanumeric characters, hyphens, and dots" unless name.match?(/\A[a-zA-Z0-9.-]+\z/)
    end

    def validate_ttl(ttl)
      raise ValidationError, "TTL must be between 1 and 3600 seconds" unless (1..3600).include?(ttl)
    end

    def handle_response(response)
      case response.status
      when 200..299
        response.body.is_a?(String) ? response.body.gsub(/^"|"$/, '') : response.body
      when 401
        raise AuthenticationError, "Invalid or missing authentication token"
      when 409
        raise LockHeldError, "Lock is already held"
      else
        error_msg = response.body.is_a?(Hash) ? response.body['error'] : "HTTP #{response.status}"
        raise LockError, error_msg
      end
    end
  end

  class OctoStoreError < StandardError; end
  class AuthenticationError < OctoStoreError; end
  class NetworkError < OctoStoreError; end
  class LockError < OctoStoreError; end
  class ValidationError < OctoStoreError; end

  class LockHeldError < LockError
    attr_reader :holder_id, :expires_at

    def initialize(message, holder_id = nil, expires_at = nil)
      super(message)
      @holder_id = holder_id
      @expires_at = expires_at
    end
  end
end