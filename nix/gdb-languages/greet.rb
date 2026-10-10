# Prints from Greeter#greet a few calls deep.
class Greeter
  def initialize(name)
    @name = name
    @counts = [1, 2, 3]
  end

  def greet(who)
    @counts << who.size
    $stdout.puts "hello from ruby, #{who} #{@name} #{@counts.inspect}"
    $stdout.flush
  end
end

g = Greeter.new("ruby")
%w[alice bob].each { |who| g.greet(who) }
