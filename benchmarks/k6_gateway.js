import http from 'k6/http';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const rate = Number(__ENV.RATE || 1000);
const duration = __ENV.DURATION || '30s';
// A VU is occupied until the response body has been validated. Provision the
// full concurrency budget before measurement: VU creation during the run both
// costs CPU and turns a slow endpoint into generator-side dropped iterations.
const vus = Number(__ENV.VUS || __ENV.CONNECTIONS || 1000);
const badStatus = new Counter('bad_status');
const badBody = new Counter('bad_body_bytes');
const requestErrors = new Counter('request_errors');
const status0 = new Counter('status_0');
const status4xx = new Counter('status_4xx');
const status5xx = new Counter('status_5xx');
export const options = {
  scenarios: { gateway: { executor: 'constant-arrival-rate', rate, timeUnit: '1s', duration, preAllocatedVUs: vus, maxVUs: vus, gracefulStop: '0s' } },
  discardResponseBodies: false,
  summaryTrendStats: ['min', 'avg', 'med', 'max', 'p(90)', 'p(95)', 'p(99)', 'p(99.9)'],
};

export default function () {
  const response = http.get(__ENV.TARGET_URL, { tags: { gateway: __ENV.TARGET_NAME || 'unknown' } });
  if (response.status !== 200) badStatus.add(1, { status: String(response.status) });
  if (response.status === 0) status0.add(1);
  if (response.status >= 400 && response.status < 500) status4xx.add(1);
  if (response.status >= 500) status5xx.add(1);
  if (!response.body || response.body.length !== 1024) badBody.add(1);
  if (response.error_code) requestErrors.add(1, { error_code: String(response.error_code) });
  check(response, { 'status 200': (r) => r.status === 200, 'body 1024 bytes': (r) => !!r.body && r.body.length === 1024 });
}
