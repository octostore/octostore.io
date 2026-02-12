Gem::Specification.new do |spec|
  spec.name          = "octostore"
  spec.version       = "1.0.0"
  spec.authors       = ["OctoStore"]
  spec.email         = ["support@octostore.io"]

  spec.summary       = "Ruby client SDK for OctoStore distributed lock service"
  spec.description   = "A Ruby client library for the OctoStore distributed lock service"
  spec.homepage      = "https://github.com/octostore/octostore-lock"
  spec.license       = "MIT"

  spec.required_ruby_version = Gem::Requirement.new(">= 2.7.0")

  spec.files         = Dir["lib/**/*", "README.md", "LICENSE"]
  spec.require_paths = ["lib"]

  spec.add_dependency "faraday", "~> 2.0"
  spec.add_dependency "json", "~> 2.0"

  spec.add_development_dependency "rspec", "~> 3.0"
  spec.add_development_dependency "webmock", "~> 3.0"
end