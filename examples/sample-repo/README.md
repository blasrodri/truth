# Sample Service

A tiny example service used by `truth`'s demos and tests.

## Runtime

The service runs on port 8080 in all environments.

## Payments

We retry payments 3 times before failing a charge. The request timeout is 30
seconds.

## Endpoints

The legacy `/v1/checkout` endpoint is deprecated and nobody uses /v1/checkout
anymore. The `/webhooks/stripe` endpoint receives provider callbacks.

## Notes

The architecture is simple and the team is happy with it. (This vague line
should NOT be extracted as a checkable claim.)
