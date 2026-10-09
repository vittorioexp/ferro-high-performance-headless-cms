// A small model in the same JSON shape returned by GET /collections.

export const COLLECTIONS = [
  {
    name: 'categories',
    meta: {},
    fields: [
      { name: 'code', type: 'string', required: true },
      { name: 'label', type: 'string' },
    ],
  },
  {
    name: 'pages',
    meta: { display_template: '{{title}}' },
    fields: [
      { name: 'title', type: 'string', required: true },
      { name: 'slug', type: 'string', required: true, unique: true },
      { name: 'status', type: 'string', default: 'draft', meta: { options: { choices: ['draft', 'published'] } } },
      { name: 'views', type: 'integer' },
      { name: 'featured', type: 'boolean' },
      { name: 'seo', type: 'json' },
      { name: 'cover', type: 'file' },
      { name: 'category', type: 'm2o', relation: { related: 'categories' } },
      { name: 'blocks', type: 'm2a', relation: { related: 'pages_blocks', foreign_field: 'pages_id', sort_field: 'sort' } },
      { name: 'tags', type: 'm2m', relation: { related: 'pages_tags', foreign_field: 'pages_id', junction_field: 'tags_id' } },
    ],
  },
  {
    name: 'pages_blocks',
    system_fields: false,
    meta: { hidden: true },
    fields: [
      { name: 'pages_id', type: 'm2o', relation: { related: 'pages', on_delete: 'cascade' } },
      { name: 'collection', type: 'string', required: true },
      { name: 'item', type: 'm2a_item', relation: { collection_field: 'collection', allowed: ['block_text', 'block_cards'] } },
      { name: 'sort', type: 'integer' },
    ],
  },
  { name: 'block_text', meta: {}, fields: [{ name: 'body', type: 'text' }] },
  {
    name: 'block_cards',
    meta: {},
    fields: [
      { name: 'title', type: 'string' },
      { name: 'cards', type: 'o2m', relation: { related: 'block_cards_items', foreign_field: 'block_cards_id', sort_field: 'sort' } },
    ],
  },
  {
    name: 'block_cards_items',
    meta: {},
    fields: [
      { name: 'block_cards_id', type: 'm2o', relation: { related: 'block_cards', on_delete: 'cascade' } },
      { name: 'title', type: 'string' },
      { name: 'sort', type: 'integer' },
    ],
  },
  {
    name: 'tags',
    meta: {},
    fields: [{ name: 'name', type: 'string' }],
  },
  {
    name: 'pages_tags',
    system_fields: false,
    meta: {},
    fields: [
      { name: 'pages_id', type: 'm2o', relation: { related: 'pages', on_delete: 'cascade' } },
      { name: 'tags_id', type: 'm2o', relation: { related: 'tags' } },
    ],
  },
];
