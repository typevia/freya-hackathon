import fixtures from './fixtures/test-customers.json' with { type: 'json' };

const requiredFields = new Map([
  ['cards.list', ['customerNo']],
  ['cards.balance', ['cardNumber']],
  ['statement.week.get', ['cardNumber']],
  ['statement.week.decide', ['cardNumber', 'newStatementWeek', 'statementDay']],
  ['statement.week.change', ['cardNumber', 'newStatementWeek', 'statementDay']],
  ['contact.email.get', ['customerNo', 'intAddressInfoType']],
  ['statement.pdf.send', ['customerNo', 'cardNumber', 'currencyType', 'statementDate', 'emailAddress']],
]);
const cardServices = ['cards.balance', 'statement.week.get', 'statement.week.decide', 'statement.week.change'];
const customers = new Map();
const cardIndexes = new Map(cardServices.map(service => [service, new Map()]));
const knownCards = new Set();

// Index service keys, not balance response masks or last-four digits. Some
// customers have card-service records even when their cards.list is an error.
for (const customer of fixtures.customers) {
  const customerNo = String(customer.customerNo);
  if (customers.has(customerNo)) throw new Error(`Duplicate customer: ${customerNo}`);
  customers.set(customerNo, customer);
  for (const service of cardServices) {
    const index = cardIndexes.get(service);
    for (const [cardNumber, record] of Object.entries(customer.services[service] ?? {})) {
      if (index.has(cardNumber)) throw new Error(`Duplicate ${service} card: ${cardNumber}`);
      index.set(cardNumber, record);
      knownCards.add(cardNumber);
    }
  }
}

const notFound = new Map([
  ['cards.list', ['FB_IVR_CHANNEL_102', 'Müşteriye ait geçerli kart bilgisi bulunamadı.']],
  ['cards.balance', ['FB_IVR_CHANNEL_120', 'Kart bakiye servisi hatası: kart bulunamadı']],
  ['statement.week.get', ['FB_IVR_CHANNEL_121', 'Kart bulunamadı']],
  ['statement.pdf.send', ['FB_IVR_CHANNEL_107', 'Pdf dokümanı bulunamadı']],
]);
const headers = { 'Content-Type': 'application/json; charset=utf-8' };
const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers });

// Stable diagnostic envelope for requests without a supplied fixture response.
function error(status, code, message, type = 'ERROR') {
  const guid = '00000000000000000000000000000000';
  return json({ result: {
    guid, returnCode: code, messageText: message, code, message, type,
    details: {
      guid,
      rootCause: status === 404 ? 'NotFoundException' : 'InvalidArgumentsException',
      errorOrigin: 'ivr-gateway-test:ivr-channel-v0',
    },
  } }, status);
}
const invalid = message => error(400, 'INVALID_ARGUMENTS_ERROR', message);
function missingRecord(service) {
  const failure = notFound.get(service);
  return failure ? error(404, ...failure, 'WARNING') : invalid('No matching fixture record');
}
function empty(value) {
  return value === undefined || value === null || (typeof value === 'string' && value.trim() === '');
}

function lookup(service, data) {
  if (service === 'cards.list' || service === 'contact.email.get') {
    return customers.get(String(data.customerNo))?.services[service];
  }
  if (service === 'statement.pdf.send') {
    if (!knownCards.has(data.cardNumber)) return undefined;
    // The fixture table dispatches PDF by customer only, not by currency,
    // statement date or recipient. In particular, do not enforce registered email.
    return customers.get(String(data.customerNo))?.services[service];
  }
  const record = cardIndexes.get(service).get(data.cardNumber);
  if (service === 'statement.week.decide' || service === 'statement.week.change') {
    const week = String(data.newStatementWeek);
    return record && Object.hasOwn(record, week) ? record[week] : undefined;
  }
  return record;
}

export default {
  async fetch(request) {
    const { pathname } = new URL(request.url);
    if (pathname === '/health' && request.method === 'GET') return json({ status: 'ok' });
    const service = pathname.startsWith('/voice/v1/') ? pathname.slice('/voice/v1/'.length) : '';
    if (!requiredFields.has(service)) return error(404, 'NOT_FOUND', 'Endpoint not found');
    if (request.method !== 'POST') return error(405, 'METHOD_NOT_ALLOWED', 'Use POST');

    let body;
    try { body = await request.json(); } catch { return invalid('Request body must be valid JSON'); }
    const data = body?.data;
    if (!data || typeof data !== 'object' || Array.isArray(data)) return invalid('data cannot be empty');
    // Check every missing field before lookup, so unknown customers/cards do not
    // mask missing arguments (including statementDay on week.change).
    for (const field of requiredFields.get(service)) {
      if (empty(data[field])) return invalid(`${field === 'cardNumber' ? 'card number' : field} cannot be empty`);
    }
    for (const field of requiredFields.get(service)) {
      const value = data[field];
      const numericAllowed = ['customerNo', 'newStatementWeek', 'statementDay'].includes(field);
      if (typeof value !== 'string' && !(numericAllowed && typeof value === 'number' && Number.isFinite(value))) {
        return invalid(`${field} has an invalid type`);
      }
    }
    if (service === 'statement.pdf.send' && !['TL', 'USD', 'EUR', 'GBP'].includes(data.currencyType)) {
      return invalid('currencyType must be TL, USD, EUR or GBP');
    }
    const record = lookup(service, data);
    if (!record) return missingRecord(service);
    if (record.delaySeconds) await new Promise(resolve => setTimeout(resolve, record.delaySeconds * 1000));
    return json(record.response, record.http);
  },
};
