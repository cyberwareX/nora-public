/**
 * Telegram EGRESS — Bot API version (grammy).
 *
 * Uses the Bot API (not GramJS/MTProto) to send replies.
 * Only needs TELEGRAM_BOT_TOKEN — no userbot session required.
 *
 * ONE tool, `reply{text}`, destination-LOCKED to the chat that woke this cycle
 * (TELEGRAM_REPLY_CHAT from harness scope_env).
 *
 * stdout is the MCP protocol channel — NEVER log to it; use stderr.
 */
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { z } from 'zod';
import { Bot } from 'grammy';

function loadToken(): string {
  const t = process.env.TELEGRAM_BOT_TOKEN;
  if (!t) {
    console.error('[telegram-bot-reply] No TELEGRAM_BOT_TOKEN env var — reply tool unavailable');
    return '';
  }
  return t.trim();
}

const token = loadToken();
let bot: Bot | null = null;

if (token) {
  bot = new Bot(token);
  console.error('[telegram-bot-reply] Bot API client initialized');
} else {
  console.error('[telegram-bot-reply] Running in no-op mode (no token)');
}

const server = new McpServer(
  {
    name: 'telegram-bot-reply',
    version: '1.0.0',
  },
  {
    capabilities: {
      tools: {},
    },
  }
);

server.tool(
  'reply',
  'Send a reply message to the Telegram chat that woke this cycle. The destination chat is locked by the harness — you cannot choose it.',
  {
    text: z.string().describe('The text to send as a reply.'),
  },
  async ({ text }: { text: string }) => {
    const chatId = process.env.TELEGRAM_REPLY_CHAT;
    const replyTo = process.env.TELEGRAM_REPLY_TO;

    if (!bot) {
      return {
        content: [
          {
            type: 'text' as const,
            text: JSON.stringify({ ok: false, error: 'No TELEGRAM_BOT_TOKEN — reply unavailable' }),
          },
        ],
      };
    }

    if (!chatId) {
      return {
        content: [
          {
            type: 'text' as const,
            text: JSON.stringify({ ok: false, error: 'No TELEGRAM_REPLY_CHAT env var — destination not set' }),
          },
        ],
      };
    }

    try {
      const opts: Record<string, unknown> = {};
      if (replyTo) {
        opts.reply_to_message_id = parseInt(replyTo, 10);
      }
      const msg = await bot.api.sendMessage(chatId, text, opts);
      return {
        content: [
          {
            type: 'text' as const,
            text: JSON.stringify({ ok: true, message_id: msg.message_id, chat_id: chatId }),
          },
        ],
      };
    } catch (e: unknown) {
      const errMsg = e instanceof Error ? e.message : String(e);
      console.error('[telegram-bot-reply] Send failed:', errMsg);
      return {
        content: [
          {
            type: 'text' as const,
            text: JSON.stringify({ ok: false, error: errMsg }),
          },
        ],
      };
    }
  }
);

const transport = new StdioServerTransport();
await server.connect(transport);
