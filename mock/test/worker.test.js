import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import fixtures from '../fixtures/test-customers.json' with { type: 'json' };
import worker from '../worker.js';

const customers = fixtures.customers;
const cardServices = ['cards.balance', 'statement.week.get', 'statement.week.decide', 'statement.week.change'];
const customerServices = ['cards.list', 'contact.email.get', 'statement.pdf.send'];
const services = [...customerServices, ...cardServices];
const known = customers[0];
const cardNumber = '540000******1234';
function request(service, data, extra = {}) {
  return new Request(`https://mock.invalid/voice/v1/${service}`, {
    method: 'POST', body: JSON.stringify({ data, ...extra }),
  });
}
async function invoke(service, data, extra) {
  const response = await worker.fetch(request(service, data, extra));
  return { status: response.status, body: await response.json() };
}
function args(customer, service, card = Object.keys(customer.services['cards.balance'] ?? {})[0], week = '2') {
  switch (service) {
    case 'cards.list': return { customerNo: customer.customerNo };
    case 'contact.email.get': return { customerNo: customer.customerNo, intAddressInfoType: 'E' };
    case 'statement.pdf.send': return {
      customerNo: customer.customerNo, cardNumber: card, currencyType: 'TL',
      statementDate: customer.services['cards.balance']?.[card]?.response.data?.statementDate ?? '05.10.2026',
      emailAddress: customer.services['contact.email.get']?.response.data?.ibanEmail || 'alternate@synthetic.example',
    };
    case 'statement.week.decide':
    case 'statement.week.change': return {
      cardNumber: card, newStatementWeek: week,
      statementDay: customer.services['statement.week.get']?.[card]?.response.data?.statementDay ?? 5,
    };
    default: return { cardNumber: card };
  }
}
async function exact(service, data, record) {
  const start = performance.now();
  const pending = invoke(service, data, { call: { ani: 'ignored', lang: 'ignored' } });
  if (record.delaySeconds) {
    const health = await worker.fetch(new Request('https://mock.invalid/health'));
    assert.equal(health.status, 200);
    assert.ok(performance.now() - start < 2000, 'delay must not block other requests');
  }
  const actual = await pending;
  assert.equal(actual.status, record.http);
  assert.deepEqual(actual.body, record.response);
  if (record.delaySeconds) assert.ok(performance.now() - start >= record.delaySeconds * 1000 - 10);
}

let vectors = 0;
for (const customer of customers) {
  for (const [service, table] of Object.entries(customer.services)) {
    if (customerServices.includes(service)) {
      vectors++;
      test(`fixture ${customer.customerNo} ${service}`, () => exact(service, args(customer, service), table));
      test(`numeric customer ${customer.customerNo} ${service}`, () => exact(service, {
        ...args(customer, service), customerNo: Number(customer.customerNo),
      }, table));
    } else {
      for (const [card, entry] of Object.entries(table)) {
        if (service.endsWith('.decide') || service.endsWith('.change')) {
          for (const [week, record] of Object.entries(entry)) {
            vectors++;
            test(`fixture ${customer.customerNo} ${service} ${card} week ${week}`, () => exact(service, args(customer, service, card, week), record));
            test(`numeric week ${customer.customerNo} ${service} ${card} week ${week}`, () => exact(service, {
              ...args(customer, service, card, week), newStatementWeek: Number(week),
            }, record));
          }
        } else {
          vectors++;
          test(`fixture ${customer.customerNo} ${service} ${card}`, () => exact(service, args(customer, service, card), entry));
        }
      }
    }
  }
}

test('fixture audit: every schema branch covered, synthetic source unchanged, no card index collisions', () => {
  assert.equal(customers.length, 34);
  assert.equal(vectors, 617);
  assert.equal(createHash('sha256').update(readFileSync(new URL('../fixtures/test-customers.json', import.meta.url))).digest('hex'), 'd6ce155638f61ab17910d53ea993a2b0d68cf4e10d6b9db2fb8e4a7add0ef44d');
  const ids = new Set();
  const counts = new Map();
  let leaves = 0;
  let delays = 0;
  function inspect(value) {
    if (Object.hasOwn(value, 'http')) {
      leaves++;
      assert.deepEqual(Object.keys(value).sort(), value.delaySeconds ? ['delaySeconds', 'http', 'response'] : ['http', 'response']);
      assert.ok([200, 404, 500].includes(value.http));
      assert.equal(value.response.result.code, value.response.result.returnCode);
      assert.equal(value.response.result.message, value.response.result.messageText);
      if (value.delaySeconds) { delays++; assert.equal(value.delaySeconds, 12); }
      return;
    }
    for (const child of Object.values(value)) inspect(child);
  }
  for (const customer of customers) {
    assert.ok(!ids.has(customer.customerNo)); ids.add(customer.customerNo);
    assert.ok(Object.keys(customer.services).every(s => services.includes(s)));
    inspect(customer.services);
    for (const service of cardServices) {
      const index = counts.get(service) ?? new Set();
      for (const card of Object.keys(customer.services[service] ?? {})) {
        assert.match(card, /^\d{6}\*{6}\d{4}$/);
        assert.ok(!index.has(card), `duplicate ${service} ${card}`); index.add(card);
      }
      counts.set(service, index);
    }
  }
  assert.equal(leaves, vectors);
  assert.equal(delays, 1);
  for (const index of counts.values()) assert.equal(index.size, 65);
});

const fields = {
  'cards.list': ['customerNo'], 'cards.balance': ['cardNumber'], 'statement.week.get': ['cardNumber'],
  'statement.week.decide': ['cardNumber', 'newStatementWeek', 'statementDay'],
  'statement.week.change': ['cardNumber', 'newStatementWeek', 'statementDay'],
  'contact.email.get': ['customerNo', 'intAddressInfoType'],
  'statement.pdf.send': ['customerNo', 'cardNumber', 'currencyType', 'statementDate', 'emailAddress'],
};
async function expectError(service, data, status, code) {
  const actual = await invoke(service, data);
  assert.equal(actual.status, status);
  assert.deepEqual(Object.keys(actual.body), ['result']);
  assert.equal(actual.body.result.code, code);
  assert.equal(actual.body.result.returnCode, code);
  assert.equal(actual.body.result.message, actual.body.result.messageText);
  assert.equal(actual.body.result.guid, actual.body.result.details.guid);
  assert.equal(actual.body.result.type, status === 404 ? 'WARNING' : 'ERROR');
  return actual;
}
for (const service of services) {
  for (const field of fields[service]) {
    for (const value of [undefined, null, '', '   ']) {
      test(`${service} required ${field}=${String(value)} takes precedence over missing records`, async () => {
        const data = { ...args(known, service), customerNo: '00000000', cardNumber: 'unknown', [field]: value };
        const actual = await expectError(service, data, 400, 'INVALID_ARGUMENTS_ERROR');
        assert.equal(actual.body.result.message, `${field === 'cardNumber' ? 'card number' : field} cannot be empty`);
        assert.deepEqual(await invoke(service, data), actual, 'deterministic error');
      });
    }
    test(`${service} rejects object or array ${field}`, async () => {
      for (const value of [{}, [], true]) await expectError(service, { ...args(known, service), [field]: value }, 400, 'INVALID_ARGUMENTS_ERROR');
    });
  }
  test(`${service} unknown record uses service-specific error`, async () => {
    const codes = { 'cards.list': 'FB_IVR_CHANNEL_102', 'cards.balance': 'FB_IVR_CHANNEL_120', 'statement.week.get': 'FB_IVR_CHANNEL_121', 'statement.pdf.send': 'FB_IVR_CHANNEL_107' };
    await expectError(service, { ...args(known, service), customerNo: '00000000', cardNumber: 'unknown' }, codes[service] ? 404 : 400, codes[service] ?? 'INVALID_ARGUMENTS_ERROR');
  });
}

for (const customer of customers) {
  for (const service of ['statement.week.decide', 'statement.week.change']) {
    for (const [card, weeks] of Object.entries(customer.services[service] ?? {})) {
      for (const week of ['0', '1', '2', '3', '4', '5', '__proto__', 'constructor']) {
        if (Object.hasOwn(weeks, week)) continue;
        test(`absent week ${customer.customerNo} ${service} ${card} ${week}`, () => expectError(service, args(customer, service, card, week), 400, 'INVALID_ARGUMENTS_ERROR'));
      }
    }
  }
  for (const card of Object.keys(customer.services['cards.balance'] ?? {})) {
    for (const currencyType of ['TL', 'USD', 'EUR', 'GBP']) {
      test(`PDF customer-only fixture ${customer.customerNo} ${card} ${currencyType}`, async () => {
        const data = { ...args(customer, 'statement.pdf.send', card), currencyType, emailAddress: 'not-registered@synthetic.example', statementDate: '01.01.2000' };
        const record = customer.services['statement.pdf.send'];
        if (record) await exact('statement.pdf.send', data, record);
        else await expectError('statement.pdf.send', data, 404, 'FB_IVR_CHANNEL_107');
      });
    }
  }
  if (!customer.services['statement.pdf.send']) {
    test(`absent PDF ${customer.customerNo}`, () => expectError('statement.pdf.send', { ...args(known, 'statement.pdf.send'), customerNo: customer.customerNo }, 404, 'FB_IVR_CHANNEL_107'));
  }
}

test('PDF unknown card fails even for known customer; invalid currency fails validation', async () => {
  await expectError('statement.pdf.send', { ...args(known, 'statement.pdf.send'), cardNumber: 'unknown' }, 404, 'FB_IVR_CHANNEL_107');
  await expectError('statement.pdf.send', { ...args(known, 'statement.pdf.send'), currencyType: 'JPY' }, 400, 'INVALID_ARGUMENTS_ERROR');
});
test('balance remasking and last-four alone are never aliases', async () => {
  for (const mask of ['5400********1234', '1234', '5400000000001234', '{{cardNumber}}', '__proto__']) {
    await expectError('cards.balance', { cardNumber: mask }, 404, 'FB_IVR_CHANNEL_120');
  }
  await expectError('cards.list', { customerNo: '{{customerNumber}}' }, 404, 'FB_IVR_CHANNEL_102');
});
test('headers, call, aiFlow and contact selector do not alter fixture', async () => {
  for (const intAddressInfoType of ['E', 'P', 'B', 'ignored']) {
    await exact('contact.email.get', { customerNo: known.customerNo, intAddressInfoType }, known.services['contact.email.get']);
  }
  const response = await worker.fetch(new Request('https://mock.invalid/voice/v1/cards.list', {
    method: 'POST', headers: { Authorization: 'Bearer ignored', 'Content-Type': 'text/plain' },
    body: JSON.stringify({ data: { customerNo: known.customerNo, aiFlow: 'ignored' }, call: null }),
  }));
  assert.deepEqual(await response.json(), known.services['cards.list'].response);
});
test('week change and PDF have no invented persistence', async () => {
  await exact('statement.week.change', args(known, 'statement.week.change'), known.services['statement.week.change'][cardNumber]['2']);
  await exact('statement.week.get', { cardNumber }, known.services['statement.week.get'][cardNumber]);
  for (let i = 0; i < 2; i++) await exact('statement.pdf.send', args(known, 'statement.pdf.send'), known.services['statement.pdf.send']);
});
test('health, exact routing, method and malformed envelope handling', async () => {
  const health = await worker.fetch(new Request('https://mock.invalid/health'));
  assert.equal(health.status, 200); assert.deepEqual(await health.json(), { status: 'ok' });
  for (const path of ['/cards.list', '/voice/v1/cards.list/', '/voice/v1/nope', '/oauth2/token']) {
    assert.equal((await worker.fetch(new Request(`https://mock.invalid${path}`))).status, 404);
  }
  assert.equal((await worker.fetch(new Request('https://mock.invalid/voice/v1/cards.list'))).status, 405);
  for (const body of ['{', 'null', '[]', '{}', '{"data":null}', '{"data":[]}', '{"data":"bad"}']) {
    const response = await worker.fetch(new Request('https://mock.invalid/voice/v1/cards.list', { method: 'POST', body }));
    assert.equal(response.status, 400); assert.equal((await response.json()).result.code, 'INVALID_ARGUMENTS_ERROR');
  }
});
