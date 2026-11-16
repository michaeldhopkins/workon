# frozen_string_literal: true

require "test_helper"

class WidgetTest < ActiveSupport::TestCase
  test "uses this workspace's database and the widgets table" do
    suffix = ENV.fetch("WORKON_RAILS_DB_SUFFIX")
    database = ActiveRecord::Base.connection_db_config.database
    assert database.end_with?(suffix), database
    assert_operator database.bytesize, :<=, 63
    assert ActiveRecord::Base.connection.table_exists?("widgets")
  end
end
