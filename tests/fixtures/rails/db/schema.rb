ActiveRecord::Schema[7.1].define(version: 1) do
  create_table :widgets do |t|
    t.string :name
    t.timestamps
  end
end
