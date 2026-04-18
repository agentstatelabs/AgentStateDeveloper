import payments, logger
# Drive logger so the tracer sees fs+time effects
logger.write_log("/tmp/asd-trace-demo.log", "hello")
# Drive a pure call (no effects)
import greetings
greetings.hello("world")
