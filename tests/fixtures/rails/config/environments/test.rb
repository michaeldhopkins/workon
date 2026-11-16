# frozen_string_literal: true

Rails.application.configure do
  config.eager_load = false
  config.secret_key_base = "workon-fixture-test-secret"
end
