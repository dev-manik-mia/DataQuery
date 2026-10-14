// Seed for the MongoDB integration tests (runs once, before the replica set starts).
const shop = db.getSiblingDB('dataquery_test');
shop.createCollection('customers', {
  validator: { $jsonSchema: { bsonType: 'object', required: ['email'], properties: { email: { bsonType: 'string' } } } },
});
shop.customers.insertMany([
  { _id: 1, email: 'ana@example.com', name: 'Ana', tier: 'gold' },
  { _id: 2, email: 'li@example.com', name: 'Li', tier: 'silver' },
  { _id: 3, email: 'zoe@example.com', name: 'Zoë', tier: null },
]);
shop.customers.createIndex({ email: 1 }, { unique: true, name: 'email_unique' });
shop.orders.insertMany([
  { _id: ObjectId('66f000000000000000000001'), customer: 1, status: 'paid', total: NumberDecimal('120.50'),
    big: NumberLong('9007199254740993'), createdAt: ISODate('2026-10-01T10:00:00Z'), items: [{ sku: 'A', qty: 2 }] },
  { _id: ObjectId('66f000000000000000000002'), customer: 2, status: 'pending', total: NumberDecimal('80.00'),
    big: NumberLong('1'), createdAt: ISODate('2026-10-02T11:30:00Z'), items: [] },
  { _id: ObjectId('66f000000000000000000003'), customer: 1, status: 'paid', total: NumberDecimal('15.25'),
    big: NumberLong('2'), createdAt: ISODate('2026-10-03T09:15:00Z'), items: [{ sku: 'B', qty: 1 }] },
]);
shop.orders.createIndex({ status: 1, createdAt: -1 }, { name: 'status_created' });
shop.createView('paid_orders', 'orders', [{ $match: { status: 'paid' } }]);
