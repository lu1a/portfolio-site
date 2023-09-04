import { json } from '@sveltejs/kit'
import { LIVE_EXPLAN_API_URL, LIVE_EXPLAN_API_TOKEN } from '$env/static/private'
 
/** @type {import('./$types').RequestHandler} */
export async function POST({ request }) {
  const { sender_address, subject, content } = await request.json()
  const data = { sender_address, subject, content }

  let responseMessage = ''

  try {
    const response = await fetch(`${LIVE_EXPLAN_API_URL}/contact`, {
      method: 'POST',
      headers: {
        'Authorization': `Bearer ${LIVE_EXPLAN_API_TOKEN}`,
      },
      body: JSON.stringify(data),
    })

    if (response.ok) {
      responseMessage = 'Message sent successfully!'
    } else {
      let wholeResponse = await response.json()
      responseMessage = wholeResponse.error
    }
  } catch (error) {
    // @ts-ignore
    responseMessage = error.message
  }
  return json(responseMessage)
}