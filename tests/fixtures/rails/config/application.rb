require_relative "boot"

require "rails"
require "active_record/railtie"

# Smallest Rails app that `db:schema:load` works against — a workon test fixture,
# not a real app. Eager loading is off so no `app/` tree is required.
module WorkonFixture
  class Application < Rails::Application
    config.load_defaults 7.1
    config.eager_load = false
  end
end
