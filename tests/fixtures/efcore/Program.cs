using System;
using Microsoft.EntityFrameworkCore;

var ctx = new AppDbContext();
ctx.Database.Migrate();

public class Widget
{
    public int Id { get; set; }
    public string? Name { get; set; }
}

public class AppDbContext : DbContext
{
    public DbSet<Widget> Widgets => Set<Widget>();

    protected override void OnConfiguring(DbContextOptionsBuilder options) =>
        options.UseNpgsql(
            Environment.GetEnvironmentVariable("ConnectionStrings__Default")
            ?? "Host=localhost;Database=placeholder");
}
